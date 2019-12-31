// Copyright (c) 2016-2018 The http-serve developers
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE.txt or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT.txt or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use super::Entity;
use crate::etag;
use crate::range;
use futures::stream::{self, StreamExt};
use futures::{self, Stream};
use http::header::{self, HeaderMap, HeaderValue};
use http::{self, Method, Request, Response, StatusCode};
use http_body::Body;
use httpdate::{fmt_http_date, parse_http_date};
use smallvec::SmallVec;
use std::io::Write;
use std::ops::Range;
use std::pin::Pin;
use std::time::SystemTime;

const MAX_DECIMAL_U64_BYTES: usize = 20; // u64::max_value().to_string().len()

fn parse_modified_hdrs(
    etag: &Option<HeaderValue>,
    req_hdrs: &HeaderMap,
    last_modified: Option<SystemTime>,
) -> Result<(bool, bool), &'static str> {
    let precondition_failed = if !etag::any_match(etag, req_hdrs)? {
        true
    } else if let (Some(ref m), Some(ref since)) =
        (last_modified, req_hdrs.get(header::IF_UNMODIFIED_SINCE))
    {
        const ERR: &'static str = "Unparseable If-Unmodified-Since";
        *m > parse_http_date(since.to_str().map_err(|_| ERR)?).map_err(|_| ERR)?
    } else {
        false
    };

    let not_modified = if !etag::none_match(&etag, req_hdrs).unwrap_or(true) {
        true
    } else if let (Some(ref m), Some(ref since)) =
        (last_modified, req_hdrs.get(header::IF_MODIFIED_SINCE))
    {
        const ERR: &'static str = "Unparseable If-Modified-Since";
        *m <= parse_http_date(since.to_str().map_err(|_| ERR)?).map_err(|_| ERR)?
    } else {
        false
    };

    Ok((precondition_failed, not_modified))
}

fn static_body<E: Entity>(
    s: &'static str,
) -> Box<dyn Stream<Item = Result<E::Data, E::Error>> + Send + Sync> {
    Box::new(stream::once(futures::future::ok(s.as_bytes().into())))
}

fn empty_body<E: Entity>() -> Box<dyn Stream<Item = Result<E::Data, E::Error>> + Send + Sync> {
    Box::new(stream::empty())
}

/// Serves GET and HEAD requests for a given byte-ranged entity.
/// Handles conditional & subrange requests.
/// The caller is expected to have already determined the correct entity and appended
/// `Expires`, `Cache-Control`, and `Vary` headers if desired.
pub fn serve<
    E: Entity,
    B: Body + From<Box<dyn Stream<Item = Result<E::Data, E::Error>> + Send + Sync>>,
    BI,
>(
    e: E,
    req: &Request<BI>,
) -> Response<B> {
    if *req.method() != Method::GET && *req.method() != Method::HEAD {
        return Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .header(header::ALLOW, HeaderValue::from_static("get, head"))
            .body(static_body::<E>("This resource only supports GET and HEAD.").into())
            .unwrap();
    }

    let last_modified = e.last_modified();
    let etag = e.etag();

    let (precondition_failed, not_modified) =
        match parse_modified_hdrs(&etag, req.headers(), last_modified) {
            Err(s) => {
                return Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .body(static_body::<E>(s).into())
                    .unwrap()
            }
            Ok(p) => p,
        };

    // See RFC 7233 section 4.1 <https://tools.ietf.org/html/rfc7233#section-4.1>: a Partial
    // Content response should include other representation header fields (aka entity-headers in
    // RFC 2616) iff the client didn't specify If-Range.
    let mut range_hdr = req.headers().get(header::RANGE);
    let include_entity_headers_on_range = match req.headers().get(header::IF_RANGE) {
        Some(ref if_range) => {
            let if_range = if_range.as_bytes();
            if if_range.starts_with(b"W/\"") || if_range.starts_with(b"\"") {
                // etag case.
                if let Some(ref some_etag) = etag {
                    if etag::strong_eq(if_range, some_etag.as_bytes()) {
                        false
                    } else {
                        range_hdr = None;
                        true
                    }
                } else {
                    range_hdr = None;
                    true
                }
            } else {
                // Date case.
                // Use the strong validation rules for an origin server:
                // <https://tools.ietf.org/html/rfc7232#section-2.2.2>.
                // The resource could have changed twice in the supplied second, so never match.
                range_hdr = None;
                true
            }
        }
        None => true,
    };

    let mut res =
        Response::builder().header(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if let Some(m) = last_modified {
        // See RFC 7232 section 2.2.1 <https://tools.ietf.org/html/rfc7232#section-2.2.1>: the
        // Last-Modified must not exceed the Date. To guarantee this, set the Date now rather than
        // let hyper set it.
        let d = SystemTime::now();
        res = res.header(header::DATE, &*fmt_http_date(d));
        let clamped_m = std::cmp::min(m, d);
        res = res.header(header::LAST_MODIFIED, &*fmt_http_date(clamped_m));
    }
    if let Some(e) = etag {
        res = res.header(http::header::ETAG, e);
    }

    if precondition_failed {
        res = res.status(StatusCode::PRECONDITION_FAILED);
        return res
            .body(static_body::<E>("Precondition failed").into())
            .unwrap();
    }

    if not_modified {
        res = res.status(StatusCode::NOT_MODIFIED);
        return res.body(empty_body::<E>().into()).unwrap();
    }

    let len = e.len();
    let (range, include_entity_headers) = match range::parse(range_hdr, len) {
        range::ResolvedRanges::None => (0..len, true),
        range::ResolvedRanges::Satisfiable(rs) => {
            if rs.len() == 1 {
                res = res.header(
                    header::CONTENT_RANGE,
                    unsafe_fmt_ascii_val!(
                        MAX_DECIMAL_U64_BYTES * 3 + "bytes -/".len(),
                        "bytes {}-{}/{}",
                        rs[0].start,
                        rs[0].end - 1,
                        len
                    ),
                );
                res = res.status(StatusCode::PARTIAL_CONTENT);
                (rs[0].clone(), include_entity_headers_on_range)
            } else {
                // Before serving multiple ranges via multipart/byteranges, estimate the total
                // length. ("80" is the RFC's estimate of the size of each part's header.) If it's
                // more than simply serving the whole entity, do that instead.
                let est_len: u64 = rs.iter().map(|r| 80 + r.end - r.start).sum();
                if est_len < len {
                    return send_multipart(e, req, res, rs, len, include_entity_headers_on_range);
                }

                (0..len, true)
            }
        }
        range::ResolvedRanges::NotSatisfiable => {
            res = res.header(
                http::header::CONTENT_RANGE,
                unsafe_fmt_ascii_val!(MAX_DECIMAL_U64_BYTES + "bytes */".len(), "bytes */{}", len),
            );
            res = res.status(StatusCode::RANGE_NOT_SATISFIABLE);
            return res.body(empty_body::<E>().into()).unwrap();
        }
    };
    res = res.header(
        header::CONTENT_LENGTH,
        unsafe_fmt_ascii_val!(MAX_DECIMAL_U64_BYTES, "{}", range.end - range.start),
    );
    let body = match *req.method() {
        Method::HEAD => empty_body::<E>(),
        _ => e.get_range(range),
    };
    let mut res = res.body(body.into()).unwrap();
    if include_entity_headers {
        e.add_headers(res.headers_mut());
    }
    res
}

enum InnerBody<D, E> {
    Once(Option<D>),
    B(Pin<Box<dyn Stream<Item = Result<D, E>> + Sync + Send>>),
}

impl<D, E> Stream for InnerBody<D, E> {
    type Item = Result<D, E>;
    fn poll_next(
        self: Pin<&mut Self>,
        ctx: &mut futures::task::Context,
    ) -> std::task::Poll<Option<Result<D, E>>> {
        // This is safe because the fields are not structurally pinned.
        // https://doc.rust-lang.org/std/pin/#pinning-is-not-structural-for-field
        // (In the B variant, the field _holds_ a pin, but it isn't itself pinned.)
        match unsafe { self.get_unchecked_mut() } {
            InnerBody::Once(ref mut o) => std::task::Poll::Ready(o.take().map(|d| Ok(d))),
            InnerBody::B(b) => b.as_mut().poll_next(ctx),
        }
    }
}

fn send_multipart<
    E: Entity,
    B: Body + From<Box<dyn Stream<Item = Result<E::Data, E::Error>> + Send + Sync>>,
    BI,
>(
    e: E,
    req: &Request<BI>,
    mut res: http::response::Builder,
    rs: SmallVec<[Range<u64>; 1]>,
    len: u64,
    include_entity_headers: bool,
) -> Response<B> {
    let mut body_len = 0;
    let mut each_part_headers = Vec::new();
    if include_entity_headers {
        let mut h = http::header::HeaderMap::new();
        e.add_headers(&mut h);
        each_part_headers.reserve(
            h.iter()
                .map(|(k, v)| k.as_str().len() + v.as_bytes().len() + 4)
                .sum::<usize>()
                + 2,
        );
        for (k, v) in &h {
            each_part_headers.extend_from_slice(k.as_str().as_bytes());
            each_part_headers.extend_from_slice(b": ");
            each_part_headers.extend_from_slice(v.as_bytes());
            each_part_headers.extend_from_slice(b"\r\n");
        }
    }
    each_part_headers.extend_from_slice(b"\r\n");

    let mut part_headers: Vec<Vec<u8>> = Vec::with_capacity(2 * rs.len() + 1);
    for r in &rs {
        let mut buf = Vec::with_capacity(64 + each_part_headers.len());
        write!(
            &mut buf,
            "\r\n--B\r\nContent-Range: bytes {}-{}/{}\r\n",
            r.start,
            r.end - 1,
            len
        )
        .unwrap();
        buf.extend_from_slice(&each_part_headers);
        body_len += buf.len() as u64 + r.end - r.start;
        part_headers.push(buf);
    }
    const TRAILER: &[u8] = b"\r\n--B--\r\n";
    body_len += TRAILER.len() as u64;

    res = res.header(
        header::CONTENT_LENGTH,
        unsafe_fmt_ascii_val!(MAX_DECIMAL_U64_BYTES, "{}", body_len),
    );
    res = res.header(
        header::CONTENT_TYPE,
        HeaderValue::from_static("multipart/byteranges; boundary=B"),
    );
    res = res.status(StatusCode::PARTIAL_CONTENT);

    if *req.method() == Method::HEAD {
        return res.body(empty_body::<E>().into()).unwrap();
    }

    // Create bodies, a stream of streams as follows: each part's header and body
    // (the latter produced lazily), then the overall trailer.
    let bodies = futures::stream::unfold(0, move |state| {
        let i = state >> 1;
        let odd = (state & 1) == 1;
        let body = if i == rs.len() && odd {
            return futures::future::ready(None);
        } else if i == rs.len() {
            InnerBody::Once(Some(TRAILER.into()))
        } else if odd {
            InnerBody::B(Pin::from(e.get_range(rs[i].clone())))
        } else {
            let v = std::mem::replace(&mut part_headers[i], Vec::new());
            InnerBody::Once(Some(v.into()))
        };
        futures::future::ready(Some((body, state + 1)))
    });

    let body = bodies.flatten();
    let body: Box<dyn Stream<Item = Result<E::Data, E::Error>> + Send + Sync> = Box::new(body);
    res.body(body.into()).unwrap()
}
