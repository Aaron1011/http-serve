# http-serve

`http-serve` is a Rust crate for serving GET and HEAD requests on read-only
entities. It's based on [hyper](https://crates.io/crates/hyper) 0.11.x and
[tokio](https://crates.io/crates/tokio). A future release is likely to switch
to the interface of the [http](http://crates.io/crates/http) crate.

It handles conditional GETs and byte range serving, taking care of the details
of HTTP `If-*` and `Range` headers, `Partial Content` responses, etc. given an
implementation of a simple trait, `Entity`. It supplies one canned `Entity`
implementation:

*   `http_serve::ChunkedReadFile` serves static files from the local
    filesystem, reading chunks in a separate thread pool to avoid blocking the
    tokio reactor thread.

You're not limited to the built-in entity type(s), though. You could supply
your own that do anything you desire:

*   bytes built into the binary via `include_bytes!`.
*   bytes retrieved from another HTTP server or network filesystem.
*   memcached-based caching of another entity.
*   anything else for which it's cheaper to compute the etag, size, and a byte
    range than the entirety of the data. (See
    [moonfire-nvr](https://github.com/scottlamb/moonfire-nvr)'s logic for
    generating `.mp4` files to represent arbitrary time ranges.)

`http-serve` is similar to golang's
[http.ServeContent](https://golang.org/pkg/net/http/#ServeContent). It was
extracted from [moonfire-nvr](https://github.com/scottlamb/moonfire-nvr)'s
`.mp4` file serving.

Try the example:

```
$ cargo run --example serve_file /usr/share/dict/words
```

## Author

Scott Lamb, slamb@slamb.org

## License

Your choice of MIT or Apache; see [LICENSE-MIT.txt](LICENSE-MIT.txt) or
[LICENSE-APACHE](LICENSE-APACHE.txt), respectively.
