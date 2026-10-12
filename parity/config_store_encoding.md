# Parity: config.rs, store.rs, encoding.rs

Source: `lua_bridge_DEV.c` (lua-bridge v0.5.4). Rust: `client/src/{config,store,encoding}.rs`.
Gate (scratch crate): `cargo test` exit 0 (36 tests), `cargo clippy --all-targets -- -D warnings` exit 0.

## Behaviours

| Behaviour | C lines | Rust item | Test |
|---|---|---|---|
| Default file text written when the ini is missing: the C's keys and defaults (4907-5003) without `loader_delay_ms` and `stack_diag`, comments rewritten for this bridge | 4907-5003 | `config::DEFAULT_INI` | `default_ini_carries_every_key_once`, `default_ini_is_ascii` |
| Per-key defaults (globals) | 557-605 | `impl Default for Config` | `default_ini_parses_to_default`, `empty_file_is_all_defaults` |
| Write the default only when no file exists; an existing file is never touched | 5005-5013 | `config::ensure_default` | `ensure_default_writes_once_and_load_reads_it` |
| Ensure-default then parse | 5015-5020 | `config::load` | `ensure_default_writes_once_and_load_reads_it` |
| Section-blind INI parse via `m2_ini_parse` rules | 5019, 4854-4857 | `config::parse` (uses `mercs2_formats::ini::parse`) | `keys_match_case_insensitively_and_last_value_wins`, `crlf_file_parses` |
| Keys case-insensitive (`_stricmp`), last value wins | 4861-4901 | `config::apply` | `keys_match_case_insensitively_and_last_value_wins` |
| Unknown keys are a startup error naming the key (the C ignored them: 4858-4902 has no else branch) | 4858-4902 | `config::apply` | `unknown_keys_are_errors` |
| All 15 keys read into typed fields | 4861-4901 | `config::Config` | `every_key_is_read` |
| `loader_source_encoding` words `auto` / `off` / `raw` | 4892-4898 | `config::EncodingMode`, `parse_encoding` | `encoding_words` |
| UTF-8 BOM stripped in every mode, with its log line | 2093-2099 | `encoding::normalize`, `Normalized::log_lines` | `utf8_bom_is_stripped_in_both_modes` |
| Raw mode / empty / pure ASCII untouched | 2111-2116 | `encoding::normalize` | `ascii_raw_and_non_utf8_pass_through` |
| Invalid UTF-8 untouched (assumed CP1252) | 2120-2121 | `encoding::normalize` | `ascii_raw_and_non_utf8_pass_through`, `mojibake_is_folded` |
| UTF-8 -> CP1252 incl. 0x80-0x9F block, Windows best fit (flags 0), `?` default | 2133-2139 | `encoding::to_cp1252`, `WCTABLE` | `every_cp1252_byte_round_trips`, `utf8_with_high_bytes_is_transcoded`, `best_fit_is_not_counted_as_replaced`, `table_is_strictly_ascending` |
| Transcode log lines (with and without the replaced-character count) | 2141-2160 | `Normalized { bytes, bom_stripped, converted, replaced }`, `Normalized::log_lines` | `utf8_with_high_bytes_is_transcoded`, `best_fit_is_not_counted_as_replaced` |
| UTF-16 BOM log line | 2100-2103 | `Utf16Source::log_line` | `utf16_bom_is_an_error` |
| Data line format `key=n:`/`b:`/`s:`, comments `;` `#`, blank lines | 3291-3312, 3335-3366 | `store::parse`, `store::render` | `render_layout_and_header`, `parse_render_round_trip`, `parse_trims_key_blanks_and_reads_lf_files` |
| File header text | 3291-3294 | `store::HEADER` | `render_layout_and_header` |
| Escape `\\` `\n` `\r` / unescape | 3222-3249 | `store::escape`, `store::unescape` | `render_layout_and_header`, `parse_render_round_trip` |
| Key leading/trailing blanks trimmed; value trailing CR/LF stripped | 3335, 3345, 3353-3356 | `store::parse` | `parse_trims_key_blanks_and_reads_lf_files` |
| Upsert: existing key updated in place, else new entry | 3252-3277 | `store::upsert`, `Store::set` | `duplicate_key_keeps_first_position_and_last_value` |
| Key / string limits (`DATA_KEY_MAX` 128 incl. NUL, `DATA_STR_MAX` 2048) | 3163-3164, 3268-3269, 3306, 3365 | `store::KEY_MAX`, `store::STR_MAX` | `set_rejects_unwritable_keys_and_values`, `malformed_lines_are_errors` |
| Missing file = empty store | 3328-3329 | `Store::load` | `missing_file_is_empty_store` |
| Write `<path>.tmp` then replace (`MOVEFILE_REPLACE_EXISTING`) | 3285-3286, 3315-3319 | `Store::persist` (`fs::rename`) | `persist_replaces_existing_file_and_round_trips_values` |
| CRLF lines on disk (MSVC text-mode `fopen(..., "w")`) | 3288 | `store::render` | `render_layout_and_header` |

## Fixes

| Fix | C behaviour (lines) | Rust | Test |
|---|---|---|---|
| C.5 #6 unparseable `host` is an error | `inet_pton` result unchecked after `memset(0)` -> binds 0.0.0.0 (4691-4695) | `parse_host` -> `ConfigError::InvalidValue` | `unparseable_host_is_an_error` |
| C.5 #6 bool keys accept exactly 0/1/true/false/on/off (any case) | `atoi` (4868-4900): `true` read as 0, `2` as on | `parse_bool` | `bool_keys_accept_exact_words_only` |
| C.5 #6 unparseable numeric value is an error naming the key | `atoi` garbage -> 0; port out of range -> silently 27050 (4865-4866); negative ms silently clamped to 0 (4874-4881) | `parse_port`, `parse_ms` | `unparseable_or_out_of_range_numbers_are_errors` |
| C.5 #6 `ConfigError` Display names key and value | n/a | `impl Display for ConfigError` | `error_display_names_key_and_value` |
| #4 keep first-saved order | New keys are prepended (3270-3271) and persist walks head-to-tail (3296), so a session writes newest-first; load prepends again, reversing file order on every load/save cycle | `Vec` in insertion order; replace in place | `order_survives_repeated_load_and_persist`, `duplicate_key_keeps_first_position_and_last_value` |
| #5 `%.9g` numbers | `%g` = 6 significant digits (3298) | `store::format_g9` (differentially checked against libc `printf("%.9g")` on 298,859 values: identical) | `g9_matches_printf`, `g9_round_trips_every_tested_f32` |
| #5 old 6-digit files still read | `atof` (3361) | `store::parse_number` | `six_digit_g_values_still_read` |
| UTF-16 BOM is an error | logged, buffer passed to the compiler unchanged (2100-2107) | `encoding::normalize` -> `Err` | `utf16_bom_is_an_error` |
| Malformed data lines are errors | silently skipped (3338-3339, 3348); unknown type creates an entry later dropped by persist (3358-3370, 3297-3312); unknown escape passed through (3243) | `store::parse` | `malformed_lines_are_errors` |
| Over-long keys/strings and NUL are errors | silently truncated (3268-3269, 3306, 3224) | `store::check_entry` | `set_rejects_unwritable_keys_and_values` |
| Persist failure is reported | `fopen` failure returns silently (3289); rename failure only logged (3316-3319) | `Store::persist` -> `Err` | `persist_replaces_existing_file_and_round_trips_values` |
| Oversized ini is an error | `m2_ini_parse` does not read files > 1 MiB (defaults used) | `ConfigError::TooLarge` | `oversized_file_is_an_error` |
| msvcrt / UCRT `%g` infinity and NaN spellings read (`1.#INF`, `-1.#INF`, `1.#IND`, `-1.#IND`, `1.#QNAN`, `-1.#QNAN`, `inf`, `-inf`, `nan`, `-nan`, `nan(ind)`, `-nan(ind)`, `nan(snan)`, `-nan(snan)`) | written by `%g` at 3298; read back by `atof` at 3361 | `store::parse_number` | `windows_crt_infinity_and_nan_spellings_read` |
