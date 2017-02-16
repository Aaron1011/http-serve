// The MIT License (MIT)
// Copyright (c) 2016 Scott Lamb <slamb@slamb.org>
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

extern crate futures;
extern crate hyper;
#[macro_use] extern crate mime;
extern crate smallvec;
extern crate time;
extern crate tokio_core;

use futures::{Future, Stream, Sink};
use futures::future;
use hyper::Error;
use hyper::server::{Request, Response};
use hyper::header;
use hyper::Method;
use smallvec::SmallVec;
use std::cmp;
use std::io::Write;
use std::ops::Range;
use tokio_core::reactor;

/// An HTTP entity for GET and HEAD serving.
pub trait Entity : 'static + Send {
    /// Returns the length of the entity in bytes.
    fn len(&self) -> u64;

    /// Gets the bytes indicated by `range`.
    fn get_range(&self, range: Range<u64>) -> hyper::Body;

    /// Adds entity headers such as `Content-Type` to the supplied `Headers` object.
    /// In particular, these headers are the "other entity-headers" described by [RFC 2616 section
    /// 10.2.7](https://tools.ietf.org/html/rfc2616#section-10.2.7); they should exclude
    /// `Content-Range`, `Date`, `ETag`, `Content-Location`, `Expires`, `Cache-Control`, and
    /// `Vary`.
    ///
    /// This function will be called only when that section says that headers such as
    /// `Content-Type` should be included in the response.
    fn add_headers(&self, &mut header::Headers);

    fn etag(&self) -> Option<header::EntityTag>;
    fn last_modified(&self) -> Option<header::HttpDate>;
}

#[derive(Debug, Eq, PartialEq)]
enum ResolvedRanges {
    None,
    NotSatisfiable,
    Satisfiable(SmallVec<[Range<u64>; 1]>)
}

fn parse_range_header(range: Option<&header::Range>, resource_len: u64) -> ResolvedRanges {
    if let Some(&header::Range::Bytes(ref byte_ranges)) = range {
        let mut ranges: SmallVec<[Range<u64>; 1]> = SmallVec::new();
        for range in byte_ranges {
            match *range {
                header::ByteRangeSpec::FromTo(range_from, range_to) => {
                    let end = cmp::min(range_to + 1, resource_len);
                    if range_from >= end {
                        continue;
                    }
                    ranges.push(Range{start: range_from, end: end});
                },
                header::ByteRangeSpec::AllFrom(range_from) => {
                    if range_from >= resource_len {
                        continue;
                    }
                    ranges.push(Range{start: range_from, end: resource_len});
                },
                header::ByteRangeSpec::Last(last) => {
                    if last >= resource_len {
                        continue;
                    }
                    ranges.push(Range{start: resource_len - last,
                                      end: resource_len});
                },
            }
        }
        if !ranges.is_empty() {
            return ResolvedRanges::Satisfiable(ranges);
        }
        return ResolvedRanges::NotSatisfiable;
    }
    ResolvedRanges::None
}

/// Returns true if `req` doesn't have an `If-None-Match` header matching `req`.
fn none_match(etag: &Option<header::EntityTag>, req: &Request) -> bool {
    match req.headers().get::<header::IfNoneMatch>() {
        Some(&header::IfNoneMatch::Any) => false,
        Some(&header::IfNoneMatch::Items(ref items)) => {
            if let Some(ref some_etag) = *etag {
                for item in items {
                    if item.weak_eq(some_etag) {
                        return false;
                    }
                }
            }
            true
        },
        None => true,
    }
}

/// Returns true if `req` has no `If-Match` header or one which matches `etag`.
fn any_match(etag: &Option<header::EntityTag>, req: &Request) -> bool {
    match req.headers().get::<header::IfMatch>() {
        // The absent header and "If-Match: *" cases differ only when there is no entity to serve.
        // We always have an entity to serve, so consider them identical.
        None | Some(&header::IfMatch::Any) => true,
        Some(&header::IfMatch::Items(ref items)) => {
            if let Some(ref some_etag) = *etag {
                for item in items {
                    if item.strong_eq(some_etag) {
                        return true;
                    }
                }
            }
            false
        },
    }
}

/// Serves GET and HEAD requests for a given byte-ranged resource.
/// Handles conditional & subrange requests.
/// The caller is expected to have already determined the correct resource and appended
/// `Expires`, `Cache-Control`, and `Vary` headers if desired.
///
/// `e` can be any of the following:
///
///    * `&'static SomeEntity`
///    * `Box<SomeEntity>`
///    * `Arc<SomeEntity>`
///
/// TODO: check HTTP rules about weak vs strong comparisons with range requests. I don't think I'm
/// doing this correctly.
pub fn serve<E: Entity>(remote: &reactor::Remote, e: E, req: &Request) -> Response {
    if *req.method() != Method::Get && *req.method() != Method::Head {
        return Response::new()
            .with_status(hyper::status::StatusCode::MethodNotAllowed)
            .with_header(header::ContentType(mime!(Text/Plain)))
            .with_header(header::Allow(vec![Method::Get, Method::Head]))
            .with_body(&b"This resource only supports GET and HEAD."[..]);
    }

    let last_modified = e.last_modified();
    let etag = e.etag();

    let precondition_failed = if !any_match(&etag, req) {
        true
    } else if let (Some(ref m), Some(&header::IfUnmodifiedSince(ref since))) =
                  (last_modified, req.headers().get()) {
        m.0.to_timespec() > since.0.to_timespec()
    } else { false };

    let not_modified = if !none_match(&etag, req) {
        true
    } else if let (Some(ref m), Some(&header::IfModifiedSince(ref since))) =
                  (last_modified, req.headers().get()) {
        m <= since
    } else { false };

    // See RFC 2616 section 10.2.7: a Partial Content response should include certain
    // entity-headers or not based on the If-Range response.
    let mut range_hdr = req.headers().get::<header::Range>();
    let include_entity_headers_on_range = match req.headers().get::<header::IfRange>() {
        Some(&header::IfRange::EntityTag(ref if_etag)) => {
            if let Some(ref some_etag) = etag {
                if if_etag.strong_eq(some_etag) {
                    false
                } else {
                    range_hdr = None;
                    true
                }
            } else {
                range_hdr = None;
                true
            }
        },
        Some(&header::IfRange::Date(ref if_date)) => {
            if let Some(ref m) = last_modified {
                // The to_timespec conversion appears necessary because in the If-Range off the
                // wire, fields such as tm_yday are absent, causing strict equality to spuriously
                // fail.
                if if_date.0.to_timespec() != m.0.to_timespec() {
                    range_hdr = None;
                    true
                } else {
                    false
                }
            } else {
                range_hdr = None;
                true
            }
        },
        None => true,
    };

    let mut res = Response::new();
    res.headers_mut().set(header::AcceptRanges(vec![header::RangeUnit::Bytes]));
    if let Some(m) = last_modified {
        // See RFC 2616 section 14.29: the Last-Modified must not exceed the Date. To guarantee
        // this, setet the Date now (if one hasn't already been set) rather than let hyper set it.
        let d = if let Some(&header::Date(header::HttpDate(d))) = res.headers().get() {
            d
        } else {
            let d = time::now_utc();
            res.headers_mut().set(header::Date(header::HttpDate(d)));
            d
        };
        res.headers_mut().set(header::LastModified(::std::cmp::min(m, header::HttpDate(d))));
    }
    if let Some(e) = etag {
        res.headers_mut().set(header::ETag(e));
    }

    if precondition_failed {
        res.set_status(hyper::status::StatusCode::PreconditionFailed);
        return res.with_body(&b"Precondition failed"[..]);
    }

    if not_modified {
        res.set_status(hyper::status::StatusCode::NotModified);
        return res;
    }

    let len = e.len();
    let (range, include_entity_headers) = match parse_range_header(range_hdr, len) {
        ResolvedRanges::None => (0 .. len, true),
        ResolvedRanges::Satisfiable(rs) => {
            if rs.len() == 1 {
                res.headers_mut().set(header::ContentRange(
                    header::ContentRangeSpec::Bytes{
                        range: Some((rs[0].start, rs[0].end-1)),
                        instance_length: Some(len)}));
                res.set_status(hyper::status::StatusCode::PartialContent);
                (rs[0].clone(), include_entity_headers_on_range)
            } else {
                // Before serving multiple ranges via multipart/byteranges, estimate the total
                // length. ("80" is the RFC's estimate of the size of each part's header.) If it's
                // more than simply serving the whole entity, do that instead.
                let est_len: u64 = rs.iter().map(|r| 80 + r.end - r.start).sum();
                if est_len < len {
                    return send_multipart(remote, e, req, res, rs, len,
                                          include_entity_headers_on_range);
                }

                (0 .. len, true)
            }
        },
        ResolvedRanges::NotSatisfiable => {
            res.headers_mut().set(header::ContentRange(
                header::ContentRangeSpec::Bytes{
                    range: None,
                    instance_length: Some(len)}));
            res.set_status(hyper::status::StatusCode::RangeNotSatisfiable);
            return res;
        }
    };
    if include_entity_headers {
        e.add_headers(res.headers_mut());
    }
    res.headers_mut().set(header::ContentLength(range.end - range.start));
    if *req.method() == Method::Head {
        return res;
    }

    res.with_body(e.get_range(range))
}

fn send_multipart<E: Entity>(remote: &reactor::Remote, e: E, req: &Request, mut res: Response,
                             rs: SmallVec<[Range<u64>; 1]>, len: u64, include_entity_headers: bool)
                             -> Response {
    let mut body_len = 0;
    let mut each_part_headers = Vec::with_capacity(128);
    if include_entity_headers {
        let mut headers = header::Headers::new();
        e.add_headers(&mut headers);
        write!(&mut each_part_headers, "{}", &headers).unwrap();
    }
    each_part_headers.extend_from_slice(b"\r\n");

    let mut part_headers: Vec<Vec<u8>> = Vec::with_capacity(2 * rs.len() + 1);
    for r in &rs {
        let mut buf = Vec::with_capacity(64 + each_part_headers.len());
        write!(&mut buf, "\r\n--B\r\nContent-Range: bytes {}-{}/{}\r\n",
               r.start, r.end - 1, len).unwrap();
        buf.extend_from_slice(&each_part_headers);
        body_len += buf.len() as u64 + r.end - r.start;
        part_headers.push(buf);
    }
    const TRAILER: &'static [u8] = b"\r\n--B--\r\n";
    body_len += TRAILER.len() as u64;

    res.headers_mut().set(header::ContentLength(body_len));
    res.headers_mut().set_raw("Content-Type", vec![b"multipart/byteranges; boundary=B".to_vec()]);
    res.set_status(hyper::status::StatusCode::PartialContent);

    if *req.method() == Method::Head {
        return res;
    }

    // Create bodies, a stream of ::hyper::Body structs as follows: each part's header and body
    // (the latter produced lazily), then the overall trailer.
    let bodies = ::futures::stream::unfold(0, move |state| {
        let i = state >> 1;
        let odd = (state & 1) == 1;
        if i == rs.len() && odd {
            None
        } else if i == rs.len() {
            Some(future::ok::<_, Error>((TRAILER.into(), state + 1)))
        } else if odd {
            Some(future::ok((e.get_range(rs[i].clone()), state + 1)))
        } else {
            Some(future::ok((::std::mem::replace(&mut part_headers[i], Vec::new()).into(),
                             state + 1)))
        }
    });

    let (sink, body) = hyper::Body::pair();
    res.set_body(body);
    let flattened = bodies.flatten().then(|i| future::ok(i));
    let send = sink.send_all(flattened).map(|_| ()).map_err(|_| ());
    match remote.handle() {
        Some(h) => h.spawn(send),
        None => remote.spawn(move |_h| send),
    }
    res
}

#[cfg(test)]
mod tests {
    use hyper::header::ByteRangeSpec;
    use hyper::header::Range::Bytes;
    use smallvec::SmallVec;
    use super::{ResolvedRanges, parse_range_header};

    /// Tests the specific examples enumerated in RFC 2616 section 14.35.1.
    #[test]
    fn test_resolve_ranges_rfc() {
        let mut v = SmallVec::new();

        v.push(0 .. 500);
        assert_eq!(ResolvedRanges::Satisfiable(v.clone()),
                   parse_range_header(Some(&Bytes(vec![ByteRangeSpec::FromTo(0, 499)])),
                                      10000));

        v.clear();
        v.push(500 .. 1000);
        assert_eq!(ResolvedRanges::Satisfiable(v.clone()),
                   parse_range_header(Some(&Bytes(vec![ByteRangeSpec::FromTo(500, 999)])),
                                      10000));

        v.clear();
        v.push(9500 .. 10000);
        assert_eq!(ResolvedRanges::Satisfiable(v.clone()),
                   parse_range_header(Some(&Bytes(vec![ByteRangeSpec::Last(500)])),
                                      10000));

        v.clear();
        v.push(9500 .. 10000);
        assert_eq!(ResolvedRanges::Satisfiable(v.clone()),
                   parse_range_header(Some(&Bytes(vec![ByteRangeSpec::AllFrom(9500)])),
                                      10000));

        v.clear();
        v.push(0 .. 1);
        v.push(9999 .. 10000);
        assert_eq!(ResolvedRanges::Satisfiable(v.clone()),
                   parse_range_header(Some(&Bytes(vec![ByteRangeSpec::FromTo(0, 0),
                                                              ByteRangeSpec::Last(1)])),
                                      10000));

        // Non-canonical ranges. Possibly the point of these is that the adjacent and overlapping
        // ranges are supposed to be coalesced into one? I'm not going to do that for now.

        v.clear();
        v.push(500 .. 601);
        v.push(601 .. 1000);
        assert_eq!(ResolvedRanges::Satisfiable(v.clone()),
                   parse_range_header(Some(&Bytes(vec![ByteRangeSpec::FromTo(500, 600),
                                                              ByteRangeSpec::FromTo(601, 999)])),
                                      10000));

        v.clear();
        v.push(500 .. 701);
        v.push(601 .. 1000);
        assert_eq!(ResolvedRanges::Satisfiable(v.clone()),
                   parse_range_header(Some(&Bytes(vec![ByteRangeSpec::FromTo(500, 700),
                                                              ByteRangeSpec::FromTo(601, 999)])),
                                      10000));
    }

    #[test]
    fn test_resolve_ranges_satisfiability() {
        assert_eq!(ResolvedRanges::NotSatisfiable,
                   parse_range_header(Some(&Bytes(vec![ByteRangeSpec::AllFrom(10000)])),
                                      10000));

        let mut v = SmallVec::new();
        v.push(0 .. 500);
        assert_eq!(ResolvedRanges::Satisfiable(v.clone()),
                   parse_range_header(Some(&Bytes(vec![ByteRangeSpec::FromTo(0, 499),
                                                              ByteRangeSpec::AllFrom(10000)])),
                                      10000));

        assert_eq!(ResolvedRanges::NotSatisfiable,
                   parse_range_header(Some(&Bytes(vec![ByteRangeSpec::Last(1)])), 0));
        assert_eq!(ResolvedRanges::NotSatisfiable,
                   parse_range_header(Some(&Bytes(vec![ByteRangeSpec::FromTo(0, 0)])), 0));
        assert_eq!(ResolvedRanges::NotSatisfiable,
                   parse_range_header(Some(&Bytes(vec![ByteRangeSpec::AllFrom(0)])), 0));

        v.clear();
        v.push(0 .. 1);
        assert_eq!(ResolvedRanges::Satisfiable(v.clone()),
                   parse_range_header(Some(&Bytes(vec![ByteRangeSpec::FromTo(0, 0)])), 1));

        v.clear();
        v.push(0 .. 500);
        assert_eq!(ResolvedRanges::Satisfiable(v.clone()),
                   parse_range_header(Some(&Bytes(vec![ByteRangeSpec::FromTo(0, 10000)])),
                                      500));
    }

    #[test]
    fn test_resolve_ranges_absent_or_invalid() {
        assert_eq!(ResolvedRanges::None, parse_range_header(None, 10000));
    }
}
