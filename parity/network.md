# Network parity: queue, raw-TCP REPL, WebSocket

Source: `mods/lua-bridge-DEV/lua_bridge_DEV.c` (v0.5.4). Rust: `client/src/queue.rs`, `client/src/repl.rs`, `client/src/ws.rs`.
Gates (run in `client/`): `cargo test --lib` exit 0 (39 of the tests are in these three modules), `cargo clippy --all-targets -- -D warnings` exit 0, `cargo check --target i686-pc-windows-gnu` exit 0.

| Behaviour | C lines | Rust item | Test |
|---|---|---|---|
| FIFO chunk queue, push from any thread, pop on the game thread | 484-494, 660-676, 699-710 | `queue::Queue::{push, pop}` | `queue::tests::push_pop_is_fifo_and_tracks_pending`, `pushes_from_many_threads_all_arrive` |
| Pending count; push reports the post-increment count (1 = empty-to-non-empty) | 693-696, 706, 1110, 1214 | `Queue::pending`, `Queue::push` return | `push_pop_is_fifo_and_tracks_pending` |
| Chunk provenance (`from_ws`, `session`; 0 = loader) | 484-490, 2717, 4607, 4822 | `queue::Origin::{Raw(u64), Ws, Loader(String)}` (loader carries the script's rel path label) | `push_pop_is_fifo_and_tracks_pending`, `ws_and_loader_results_never_reach_the_raw_buffer` |
| Deliver to raw TCP only when raw and the session is attached; result then `<<<END>>>`, each + `\n` | 637-655, 1173-1176 | `Queue::deliver` -> `Delivery::Delivered` | `current_session_result_is_framed_with_end_marker`, `wire_bytes_queued_then_result_then_end` |
| Stale raw result dropped with a log line | 1177-1181 | `Delivery::DroppedStale` | `stale_session_result_is_dropped_and_logged`, `result_for_a_departed_client_is_not_sent_to_the_next` |
| WS / loader result never written to the raw buffer | 1164-1172 | `Delivery::NotRaw` | `ws_and_loader_results_never_reach_the_raw_buffer` |
| New session id per raw client; leftover output cleared | 4728-4742 | `Queue::begin_session`, `Queue::end_session` | `session_boundaries_discard_leftover_output` |
| Bind `host:port`, listen, accept loop; accept error -> continue | 4685-4720 | `repl::start`, `accept_loop` | every `repl::tests` socket test |
| Peek first bytes; `GET ` -> WebSocket when WS enabled, else raw | 4755-4777 | `repl::connection` (`ws: Option<WsHub>`) | `ws_request_ack_and_queue`, `get_is_raw_tcp_when_ws_is_disabled` |
| One raw client at a time; a second waits until the first leaves | 4704-4848 | `Ctx::raw_slot` | `second_raw_client_waits_for_the_first` |
| Lines split on `\n`, one trailing `\r` stripped | 4812-4815 | `repl::Framer::feed` | `framer_lines_crlf_empty_run_and_cap` |
| `<<<RUN>>>` must be a whole line; empty chunk -> nothing queued, nothing sent | 4816-4825 | `Framer::feed` | `framer_lines_crlf_empty_run_and_cap` |
| `[queued]\n` acknowledgement | 4823 | `repl::QUEUED_ACK` | `wire_bytes_queued_then_result_then_end` |
| Chunk limit: `len + line + 1 < 1048576` (max chunk 1048575 bytes) | 4709, 4828 | `repl::CHUNK_BUFFER` | `framer_accepts_a_chunk_just_under_the_cap` |
| Line limit: 65535 unterminated bytes ends the connection | 4705-4708, 4804-4805 | `repl::RX_BUFFER`, `Framer::room` | `oversize_line_is_rejected_and_closes` |
| Output flushed every iteration; 50 ms input poll | 4779-4801 | `serve_raw` (`take_output`, read timeout `POLL`) | `wire_bytes_queued_then_result_then_end` |
| Send failure on flush ends the session | 4787-4792 | `serve_raw` `RawEnd::Io` | (no deterministic test) |
| Accept key = base64(sha1(key + GUID)) | 3964-3966, 3986-4019, 4254-4260 | `ws::accept_key` | `accept_key_matches_rfc6455_example` |
| Handshake: read to `\r\n\r\n` (8191-byte limit), case-insensitive `Sec-WebSocket-Key:` at line start, skip spaces/tabs, key 1..=127 bytes, exact 101 response | 4200-4271 | `ws::read_handshake_key`, `find_key`, `handshake_response` | `handshake_key_is_found_case_insensitively_after_whitespace`, `handshake_without_key_or_with_oversize_headers_fails` |
| Server frames: FIN + opcode, 7/16/64-bit length, unmasked | 4059-4095 | `ws::encode_frame` | `frame_length_encodings` |
| Client frames must be masked | 4137-4139 | `ws::recv_message` | `recv_protocol_violations`, `unmasked_ws_frame_closes_the_connection` |
| 64-bit length over 1 MiB rejected; assembled message must be < 1 MiB | 3976-3978, 4127-4135, 4180 | `ws::MESSAGE_BUFFER` | `recv_protocol_violations` |
| Control frames <= 125 bytes; ping -> pong; close echoed then end; pong ignored | 4146-4167 | `recv_message` `control` callback | `recv_assembles_fragments_and_answers_ping_mid_message`, `recv_echoes_close_and_reports_closed`, `ws_request_ack_and_queue` |
| Fragmentation: first text/binary, then continuation only | 4169-4189 | `recv_message` | `recv_assembles_fragments_and_answers_ping_mid_message`, `recv_protocol_violations` |
| JSON string decode for `code` (`\" \\ \/ \n \r \t \b \f \uXXXX`, >0xFF -> `?`), caps key 32 / id 128 / unknown 256 | 4288-4343, 4360, 4385-4387, 4585 | `ws::parse_string` | `parse_request_rules` |
| `{id, code}` request, unknown keys skipped, both required | 4345-4401 | `ws::parse_request` | `parse_request_rules` |
| Malformed request: logged, connection stays open | 4602-4605 | `serve_client` | `ws_request_ack_and_queue` |
| WS chunk queued as `from_ws` with session 0 | 4607-4610 | `ws::dispatch` (`Origin::Ws`) | `dispatch_normalises_and_queues_as_ws`, `ws_request_ack_and_queue` |
| Ack `{"type":"ack","id":"…","status":"queued"}` | 4612-4621 | `ws::ack_queued` | `ack_escapes_the_id_as_utf8`, `ws_request_ack_and_queue` |
| Broadcast JSON escape; CP1252 high bytes -> `\uXXXX` (lowercase hex) | 4403-4458 | `ws::escape_json` | `typed_line_escapes_cp1252_and_controls` |
| Broadcast `{"type":"<kind>","line":"…"}` to every client; failed client closed and removed | 4499-4553 | `WsHub::broadcast` (hub is `Clone`), per-client `sender` | `ws_request_ack_and_queue`, `broadcast_through_a_cloned_hub_reaches_a_client` |
| 16 clients maximum | 622, 4465-4486, 4633-4650 | `ws::MAX_CLIENTS` | `ws_seventeenth_client_gets_503_before_any_101` |
| Frame writes to one socket never interleave | 4055-4057, 4154-4162, 4615-4620, 4544-4552 | per-client `writer` mutex (never taken by a broadcaster) | `ws_request_ack_and_queue` |

## Fixes

- **FIX #3** (4612-4613): the ack's `id` was formatted raw with `%s`; a `"` in the id produced invalid JSON / injected keys. The id is now decoded as JSON text (`parse_string_utf8`: raw UTF-8, `\uXXXX` with surrogate pairs; invalid UTF-8 or a lone surrogate rejects the request) and re-escaped as UTF-8 (`escape_json_utf8`), so a non-ASCII id round-trips. Broadcast lines stay CP1252. Tests: `non_ascii_id_round_trips_through_parse_and_ack`, `id_decodes_as_unicode_text`, `ws_request_ack_and_queue`.
- **FIX #9** (596-600 vs 4607, 4822): only file loads went through `NormalizeSourceEncoding` (2206). Raw-TCP and WS chunks now pass through the caller's fallible `Normalize` before queueing. On `Err(why)` nothing is queued; raw gets `[bridge] chunk rejected: <why>\n<<<END>>>\n` (no `[queued]`); WS gets `{"type":"ack","id":"…","status":"rejected","error":"<why>"}` plus `{"type":"log","line":"[!] lua_bridge: ws: chunk id=<id> rejected: <why>"}` on the feed. Tests: `normalize_error_rejects_the_chunk_on_the_wire`, `ws_normalize_error_acks_rejected_and_logs_to_the_feed`, `dispatch_rejection_queues_nothing`.
- **FIX #11** (4781-4794, 4499-4553): the REPL thread held `g_outMtx` across `send`; broadcasts ran `send` on the game thread under `g_wsClientMtx`. Now `take_output` swaps the buffer out and sends unlocked, and `broadcast` only appends to per-client outboxes drained by one sender thread per client. Test: `stalled_client_is_dropped_at_the_backlog_bound_without_stalling_others`.
- **Bounded broadcast backlog**: each client may have at most `ws::BACKLOG_BYTES` = 4 MiB of encoded frames queued. A broadcast that would exceed it disconnects that client with a log line naming its id and peer address (`broadcast backlog full …`). No silent drops, no unbounded growth. Test: `stalled_client_is_dropped_at_the_backlog_bound_without_stalling_others` (a healthy client receives all 4000 lines while the stalled one is dropped).
- **Accept loop never blocks on a client** (4719-4777: peek, handshake and the raw session all ran on the accept thread). Every connection gets its own thread. With WS enabled, the first bytes and the whole handshake must arrive within `repl::FIRST_BYTES_TIMEOUT` (5 s); a silent connection is closed and logged. Raw sessions are still one at a time, but the wait is on the connection's thread, so WS clients connect while a REPL is attached. Tests: `ws_connects_while_a_raw_session_is_attached`, `silent_connection_is_closed_after_the_timeout`, `second_raw_client_waits_for_the_first`.
- **17th WS client refused before the 101** (4636-4640 refused after the 101 was sent): it now gets `HTTP/1.1 503 Service Unavailable` with `Content-Type: text/plain`, `Content-Length`, `Connection: close` and the body `lua_bridge: 16 WebSocket clients already connected\n`, then the socket is closed and the refusal logged.
- **Session lifecycle**: session check and append are one critical section (1173-1176 vs 4728-4742 raced). A session is detached when its client leaves, so a late result is dropped and logged instead of sitting in the buffer. Session ids are `u64` (`LONG` wraps).
- `[queued]` is buffered before the push (4822-4823 let a result precede it).
- Over-cap chunk (4828-4832: lines silently discarded, truncated chunk executed): rejected at RUN with `[bridge] chunk rejected: larger than the 1048575-byte chunk limit\n<<<END>>>\n`.
- Over-long line (4705-4708: dropped silently): `[bridge] line rejected: 65535 bytes without a newline\n<<<END>>>\n`, then close.
- Broadcast line size (4507-4535 silently dropped lines over ~12 KB escaped): no cap; the per-client backlog bound applies instead.
- Handshake headers over 8191 bytes (4221-4229 parsed whatever it had): handshake error.
- Bind failure (4696-4701) is returned from `repl::start`.
- A zero-length WS message (4597-4598 treated `r <= 0` as close) is a malformed request (logged, connection kept).
- Decoded NUL in `id` / `code` (4607 used `strlen`): request rejected.
- WS protocol errors, handshake failures and disconnect reasons are logged.

## Differences that are not fixes

- **SO_REUSEADDR** (4690): not set. On Windows it lets another process bind the same port while the bridge holds it. Rust's std does not set it on Windows; on Unix std's `TcpListener::bind` sets it (Unix semantics only allow rebinding a TIME_WAIT port). Listen backlog is std's default, not 8 (4696). No windows-sys feature was added.
- `peek` needs 4 bytes to recognise `GET ` (4757-4758, `pn >= 4`); a client whose first segment is shorter is treated as raw, as before.
- Bytes after `\r\n\r\n` in the handshake read are discarded, as before (4221-4229).
