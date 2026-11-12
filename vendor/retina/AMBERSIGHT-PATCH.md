# Local patches to retina 0.4.21 (MIT/Apache-2.0)

`src/client/mod.rs`, `Session::describe_with_conn`: send an `OPTIONS` request
on the connection before `DESCRIBE` (errors ignored), reusing its auth state.

Why: a Reolink Home Hub only streams its battery cameras to a client that sent
OPTIONS first on the same connection (verified 2026-10-31: with OPTIONS and both
tracks set up, 418 KB in 6 s; without OPTIONS, 104 bytes). Worth proposing
upstream; drop this patch once released there.

Later finding (same day): even with this patch, the hub's RTSP for a battery
camera only produced video on roughly 20-50% of attempts (the rest get RTCP
but no RTP), so the single test above may have been luck rather than proof
that OPTIONS is required. The patch is harmless (OPTIONS is a standard
request) and is kept for now; the hub's HTTP-FLV stream was reliable (5/5).

## Patch 2 (2026-11-05): recover from framing errors instead of ending the session

Some Reolink cameras (wired and Wi-Fi) logged hundreds of
`RTSP framing error: Invalid RTSP message ... request-line` an hour: where a
message should start there is video data, so the session ended and the camera
was gone for 2-8 s while it reconnected. The same cameras read from another
machine (Linux and Windows, plain and with AmberSight's setup) stayed clean,
so the cause is still being looked for; the log line below is there to find it.

- `src/tokio.rs`: on a parse error the connection skips forward to the next
  certain message start (an interleaved header with a small channel id, an RTP
  version-2 first byte and another header right after its packet, or
  `RTSP/1.0 `) and continues. Gives up with the original error after 4 MB /
  5 s without a start, or after 30 recoveries in a minute. Logs
  `RTSP stream out of step from <peer>: skipped N bytes ...` with the last four
  interleaved headers. Tests: `resync_*`, `connection_recovers_from_garbage`.
- `src/rtsp/parse.rs`: `Parser::resync_to`.
- `src/client/mod.rs`: a recovery may skip a keepalive answer, so the next
  keepalive tick after one does not count the missing answer, and a late
  answer to an earlier keepalive is ignored instead of ending the session.
- `src/testutil.rs`: `init_logging` no longer uses upstream's `mylog`
  dev-dependency (not in the published crate) so the tests build.

## Patch 3 (2026-11-05): receive buffer, RTCP receiver reports

Cause of patch 2's framing errors, found: a Reolink camera that is not read
for ~2 s on Windows (default TCP receive window) cuts packets short; with an
8 MB receive buffer, pauses up to 4 s did no harm (Linux autotuned buffers
were never affected).

- `src/tokio.rs` + `src/lib.rs`: `retina::set_tcp_recv_buffer(bytes)` sets
  SO_RCVBUF on RTSP connections made afterwards (before connect, so the
  window can open that far); AmberSight sets 8 MB on Windows.
- `src/rtcp.rs` `receiver_report`, sent from `src/client/mod.rs` with each
  keepalive (every half session timeout, <= 30 s): one compound RR + SDES
  CNAME per playing TCP stream on its RTCP channel. Upstream sent none; a
  live555-based Kuna camera ends a session after minutes without RTCP from
  the client even while RTSP keepalives are answered. Accepted by Reolink and
  Amcrest/Dahua cameras.

Known: 7 `client::tests` fail or hang since patch 1 (their mock servers expect
DESCRIBE first, not OPTIONS); not caused by patch 2.

## Patch 4 (2026-11-08): a late keepalive answer does not end a session while video arrives

A wired camera dropped 25-32 times a day with `Server failed to respond to
keepalive N within 5s` although video was flowing: its control channel is
occasionally slow to answer. `src/client/mod.rs`, `handle_keepalive_timer`:
in state `Waiting` at the next tick, if an RTP packet was delivered since the
previous tick (`media_since_tick`, set in `poll_stream_item`) and the keepalive
has been outstanding for less than 60 s (`KEEPALIVE_LATE_MAX`), the timer is
re-armed and the session continues. No second keepalive is sent while one is
outstanding (so this can only remove reconnects, never add requests; the
camera already has the request, it is only slow to answer). The late answer is
matched by CSeq in `handle_response` as before; older CSeqs are ignored. With
no RTP in the last interval, or after 60 s, behaviour is unchanged (session
ends). Logged once per session at info, later ones at debug. Tests:
`late_keepalive_*` in `client::tests` (the other mock-server tests that were
already failing since patch 1 still do).

## License

retina is Copyright Scott Lamb <slamb@slamb.org> and contributors, dual
licensed under MIT or Apache-2.0 at your option (see `Cargo.toml`). The
license texts are in the upstream repository,
https://github.com/scottlamb/retina (`LICENSE-MIT`, `LICENSE-APACHE`); they
were not included in the published crate this copy was taken from. Add them
here before distributing builds.
