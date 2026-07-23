//! OSC 778 query channel: the wire envelope shared by terminal and clients.
//!
//! Where OSC 777 carries *commands* (fire-and-forget control), OSC 778
//! carries *queries and replies* — the read side of the protocol plus the
//! return path for opt-in command acks. The wire form is a single OSC
//! sequence in each direction, ST-terminated:
//!
//! ```text
//! query:  ESC ] 778 ; v=1 ; t=q ; id=<token> ; op=<op> ; data=<b64url-json> ESC \
//! reply:  ESC ] 778 ; v=1 ; t=r ; id=<token> [; kind=ack] ; ok=1|0 [; code=<error>] [; data=<b64url-json>] ESC \
//! ```
//!
//! Envelope fields are strict ASCII metadata; all structured or
//! user-controlled content lives inside the unpadded base64url JSON
//! payload, so there are no escaping questions. Correlation is by token;
//! replies go only to the originating transport. `t=e` is reserved for a
//! future subscription protocol and is not emitted in v1.
//!
//! This module is dependency-free (std only) so the `ratty-ai` CLI can
//! include it verbatim, the same way it includes `osc.rs` — the envelope
//! codec can then never drift between the terminal and its clients.

/// OSC numeric code claimed by the ratty query channel.
pub const RATTY_QUERY_OSC: &[u8] = b"778";

/// Envelope protocol version emitted and accepted by this build.
pub const QUERY_VERSION: &str = "1";

/// Upper bound on a *terminated* inbound OSC 778 sequence that the
/// terminal will accept and decode; anything larger answers `too-large`.
///
/// This is the query-acceptance bound. Memory against never-terminated or
/// gigabyte-long *OSC* input is bounded separately by the OSC watchdog in
/// `crate::inline`, which caps how many bytes of any single OSC sequence
/// reach vte's unbounded `std` buffer; that cap sits above this one so a
/// valid-but-oversized query still reaches this check and is answered
/// `too-large` rather than truncated. The APC channel accumulates through
/// a different path and is bounded independently, by
/// `inline::MAX_APC_SEQUENCE_BYTES`.
pub const MAX_QUERY_SEQUENCE_BYTES: usize = 8 * 1024;

/// Upper bound on a decoded query `data=` payload.
pub const MAX_QUERY_DATA_BYTES: usize = 4 * 1024;

/// Upper bound on a whole outbound reply sequence. Replies are written to
/// the PTY with a blocking write on the render thread, so this is a
/// liveness bound, not just a wire nicety; larger collections paginate.
pub const MAX_REPLY_SEQUENCE_BYTES: usize = 4 * 1024;

/// Upper bound on a correlation token, in bytes.
pub const MAX_TOKEN_BYTES: usize = 64;

/// Stable error codes carried in the reply `code=` field.
///
/// Codes are append-only: new codes may be added, existing codes never
/// change meaning. Clients treat unknown codes as generic failures.
pub mod codes {
    /// The envelope is missing required fields, carries a malformed field,
    /// or contains non-ASCII bytes.
    pub const BAD_ENVELOPE: &str = "bad-envelope";
    /// The envelope `v=` is not a version this build speaks.
    pub const BAD_VERSION: &str = "bad-version";
    /// The sequence or its decoded payload exceeds a size limit.
    pub const TOO_LARGE: &str = "too-large";
    /// The `data=` payload is not valid unpadded base64url JSON.
    pub const BAD_PAYLOAD: &str = "bad-payload";
    /// The query `op=` is not supported by this build (see `caps`).
    pub const UNSUPPORTED_OP: &str = "unsupported-op";
    /// The command parsed but its subsystem is not built yet.
    pub const UNSUPPORTED: &str = "unsupported";
    /// A `tok=`-carrying OSC 777 sequence did not parse into a command.
    pub const BAD_COMMAND: &str = "bad-command";
    /// A pagination cursor is malformed, foreign, or from a past session.
    pub const BAD_CURSOR: &str = "bad-cursor";
    /// The target id lies outside the caller's namespace.
    pub const NOT_OWNER: &str = "not-owner";
    /// No live object exists under the target id.
    pub const UNKNOWN_ID: &str = "unknown-id";
    /// The object exists but scrolled away and has no anchor.
    pub const NO_ANCHOR: &str = "no-anchor";
    /// The id already names a live object (and `replace` was not set).
    pub const ALREADY_EXISTS: &str = "already-exists";
    /// The id was already used this session; ids are never reused.
    pub const ID_REUSED: &str = "id-reused";
    /// The session's distinct-id budget is exhausted.
    pub const SESSION_BUDGET: &str = "session-budget";
    /// The caller's namespace is at its live-object cap.
    pub const NAMESPACE_CAP: &str = "namespace-cap";
    /// The asset name is not a loadable embedded asset.
    pub const BAD_ASSET: &str = "bad-asset";
    /// The mode string is not a known presentation mode.
    pub const BAD_MODE: &str = "bad-mode";
    /// The kind is not a registered semantic kind for the requested op.
    /// NOTE: the M3.5 viz lane appends an identical `bad-kind` constant —
    /// keep the name and value byte-identical so the branches merge
    /// trivially.
    pub const BAD_KIND: &str = "bad-kind";
    /// A one-shot was requested while audio is locked (browser autoplay
    /// policy, pre-gesture); the sound did not and will not play.
    pub const AUDIO_LOCKED: &str = "audio-locked";
    /// Qualifier on an `ok=1` ack: the ambient request committed as
    /// retained state and fades in after the first user gesture unlocks
    /// audio. There is no later notification — clients poll `state.scene`.
    pub const DEFERRED: &str = "deferred";
    /// The caller exceeded its per-namespace one-shot rate limit.
    pub const RATE_LIMITED: &str = "rate-limited";
    /// The one-shot voice caps (global or per-namespace) are full.
    pub const VOICE_CAP: &str = "voice-cap";
    /// The caller's ingress tier does not carry the required capability
    /// (e.g. scene ambient audio is disabled by trusted config).
    pub const NOT_PERMITTED: &str = "not-permitted";
    /// The query timed out client-side (never emitted by the terminal).
    pub const TIMEOUT: &str = "timeout";
    /// The session was disposed with the query outstanding.
    pub const DISPOSED: &str = "disposed";
    /// An internal invariant failed while answering.
    pub const INTERNAL: &str = "internal";
}

/// Returns whether `token` is a valid correlation token: 1 to
/// [`MAX_TOKEN_BYTES`] characters of the base64url alphabet.
pub fn valid_token(token: &str) -> bool {
    !token.is_empty() && token.len() <= MAX_TOKEN_BYTES && token.bytes().all(is_b64url_byte)
}

/// Returns whether `op` may be spliced into a query envelope: non-empty
/// printable ASCII with no `;` (which would inject envelope fields) and
/// no `=` ambiguity in the leading position. Clients check this before
/// emitting so garbage never reaches the wire.
pub fn valid_op(op: &str) -> bool {
    !op.is_empty()
        && !op.starts_with('=')
        && op
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && byte != b';')
}

fn is_b64url_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'
}

/// Encodes bytes as unpadded base64url (RFC 4648 §5, no `=` padding).
pub fn b64url_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(triple >> 18) as usize & 0x3F] as char);
        out.push(ALPHABET[(triple >> 12) as usize & 0x3F] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(triple >> 6) as usize & 0x3F] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[triple as usize & 0x3F] as char);
        }
    }
    out
}

/// Why an unpadded base64url decode failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum B64DecodeError {
    /// A byte outside the base64url alphabet.
    BadChar,
    /// An input length no unpadded encoding produces (4n+1).
    BadLength,
    /// The decoded output would exceed the caller's limit.
    TooLarge,
}

/// Decodes unpadded base64url, refusing outputs larger than `max_decoded`.
///
/// # Errors
///
/// Returns a [`B64DecodeError`] for non-alphabet bytes, impossible lengths,
/// or outputs over the limit; never panics on untrusted input.
pub fn b64url_decode(input: &str, max_decoded: usize) -> Result<Vec<u8>, B64DecodeError> {
    let bytes = input.as_bytes();
    if bytes.len() % 4 == 1 {
        return Err(B64DecodeError::BadLength);
    }
    let decoded_len = bytes.len() / 4 * 3
        + match bytes.len() % 4 {
            0 => 0,
            2 => 1,
            3 => 2,
            _ => unreachable!("length 4n+1 rejected above"),
        };
    if decoded_len > max_decoded {
        return Err(B64DecodeError::TooLarge);
    }
    let mut out = Vec::with_capacity(decoded_len);
    for chunk in bytes.chunks(4) {
        let mut acc: u32 = 0;
        for &byte in chunk {
            acc = (acc << 6) | u32::from(b64url_value(byte).ok_or(B64DecodeError::BadChar)?);
        }
        match chunk.len() {
            4 => out.extend_from_slice(&[(acc >> 16) as u8, (acc >> 8) as u8, acc as u8]),
            3 => {
                acc <<= 6;
                out.extend_from_slice(&[(acc >> 16) as u8, (acc >> 8) as u8]);
            }
            2 => {
                acc <<= 12;
                out.push((acc >> 16) as u8);
            }
            _ => unreachable!("length 4n+1 rejected above"),
        }
    }
    Ok(out)
}

fn b64url_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'-' => Some(62),
        b'_' => Some(63),
        _ => None,
    }
}

/// A well-formed query envelope received by the terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryEnvelope {
    /// Client-generated correlation token.
    pub token: String,
    /// Query op (e.g. `state.scene`, `caps`).
    pub op: String,
    /// Decoded JSON payload bytes; empty when the query carried no `data=`.
    pub data: Vec<u8>,
}

/// An error reply owed for a sequence that failed at the parse layer,
/// before any command or query could execute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireErrorReply {
    /// The token to correlate the error to.
    pub token: String,
    /// The error code (from [`codes`]).
    pub code: &'static str,
    /// Whether the reply is a command ack (`kind=ack`) rather than a query
    /// reply.
    pub ack: bool,
}

/// An OSC 778 sequence classified at terminal ingress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Wire778 {
    /// A well-formed query.
    Query(QueryEnvelope),
    /// A reply envelope (`t=r`). The terminal never consumes replies — a
    /// reply echoed back through the output stream is silently dropped.
    ReplyEcho,
    /// A malformed sequence; `token` is present when it could be recovered
    /// so an error reply can still be correlated.
    Malformed {
        /// Best-effort recovered token.
        token: Option<String>,
        /// The error code (from [`codes`]).
        code: &'static str,
    },
}

/// Classifies an OSC sequence delivered by vt100 as `;`-split params.
///
/// Returns `None` for any OSC not claiming code `778`, so unrelated OSC
/// users pass through untouched. The envelope grammar guarantees no field
/// contains `;` (tokens and payloads use the base64url alphabet), so
/// vt100's param split is lossless and each param is one `k=v` field.
pub fn parse_778(params: &[&[u8]]) -> Option<Wire778> {
    let first = params.first()?;
    if *first != RATTY_QUERY_OSC {
        return None;
    }

    let total: usize = params.iter().map(|p| p.len() + 1).sum();
    let mut version = None;
    let mut kind = None;
    let mut token = None;
    let mut op = None;
    let mut data = None;
    let mut malformed = false;

    for field in &params[1..] {
        if !field.iter().all(|b| b.is_ascii_graphic()) {
            malformed = true;
            continue;
        }
        // Fields are ASCII-checked above, so the conversion is lossless.
        let field = String::from_utf8_lossy(field);
        let Some((key, value)) = field.split_once('=') else {
            malformed = true;
            continue;
        };
        match key {
            "v" => version = Some(value.to_string()),
            "t" => kind = Some(value.to_string()),
            "id" => token = Some(value.to_string()),
            "op" => op = Some(value.to_string()),
            "data" => data = Some(value.to_string()),
            // Unknown envelope keys are ignored so the envelope can evolve
            // additively within a version.
            _ => {}
        }
    }

    let token = token.filter(|t| valid_token(t));
    let fail = |code| {
        Some(Wire778::Malformed {
            token: token.clone(),
            code,
        })
    };

    if total > MAX_QUERY_SEQUENCE_BYTES {
        return fail(codes::TOO_LARGE);
    }
    if kind.as_deref() == Some("r") {
        return Some(Wire778::ReplyEcho);
    }
    if malformed || token.is_none() {
        return fail(codes::BAD_ENVELOPE);
    }
    if version.as_deref() != Some(QUERY_VERSION) {
        return fail(codes::BAD_VERSION);
    }
    if kind.as_deref() != Some("q") {
        return fail(codes::BAD_ENVELOPE);
    }
    let Some(op) = op.filter(|op| !op.is_empty()) else {
        return fail(codes::BAD_ENVELOPE);
    };
    let data = match data {
        None => Vec::new(),
        Some(encoded) => match b64url_decode(&encoded, MAX_QUERY_DATA_BYTES) {
            Ok(decoded) => decoded,
            Err(B64DecodeError::TooLarge) => return fail(codes::TOO_LARGE),
            Err(_) => return fail(codes::BAD_PAYLOAD),
        },
    };

    Some(Wire778::Query(QueryEnvelope {
        // Valid by the filter above.
        token: token.unwrap_or_default(),
        op,
        data,
    }))
}

/// Builds the full ST-terminated OSC 778 query sequence a client emits.
pub fn query_sequence(token: &str, op: &str, data_json: Option<&[u8]>) -> String {
    let mut out = format!("\x1b]778;v={QUERY_VERSION};t=q;id={token};op={op}");
    if let Some(data) = data_json {
        out.push_str(";data=");
        out.push_str(&b64url_encode(data));
    }
    out.push_str("\x1b\\");
    out
}

/// Builds the full ST-terminated OSC 778 reply sequence the terminal emits.
pub fn reply_sequence(
    token: &str,
    ack: bool,
    ok: bool,
    code: Option<&str>,
    data_json: Option<&[u8]>,
) -> Vec<u8> {
    let mut out = format!("\x1b]778;v={QUERY_VERSION};t=r;id={token}");
    if ack {
        out.push_str(";kind=ack");
    }
    out.push_str(if ok { ";ok=1" } else { ";ok=0" });
    if let Some(code) = code {
        out.push_str(";code=");
        out.push_str(code);
    }
    if let Some(data) = data_json {
        out.push_str(";data=");
        out.push_str(&b64url_encode(data));
    }
    out.push_str("\x1b\\");
    out.into_bytes()
}

/// A reply envelope parsed by a client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedReply {
    /// Correlation token.
    pub token: String,
    /// Whether the reply is a command ack.
    pub ack: bool,
    /// Success flag.
    pub ok: bool,
    /// Error code when `ok` is false.
    pub code: Option<String>,
    /// Decoded JSON payload bytes; empty when the reply carried none.
    pub data: Vec<u8>,
}

/// Parses a reply envelope body (the bytes between `ESC ] 778 ;` and the
/// terminator). Returns `None` for anything that is not a well-formed
/// v-matching reply — clients ignore unmatched or malformed frames.
pub fn parse_reply_body(body: &str) -> Option<ParsedReply> {
    let mut version = None;
    let mut kind = None;
    let mut token = None;
    let mut reply_kind = None;
    let mut ok = None;
    let mut code = None;
    let mut data = None;
    for field in body.split(';') {
        let (key, value) = field.split_once('=')?;
        match key {
            "v" => version = Some(value),
            "t" => kind = Some(value),
            "id" => token = Some(value.to_string()),
            "kind" => reply_kind = Some(value.to_string()),
            "ok" => ok = Some(value == "1"),
            "code" => code = Some(value.to_string()),
            "data" => data = Some(value.to_string()),
            _ => {}
        }
    }
    if version != Some(QUERY_VERSION) || kind != Some("r") {
        return None;
    }
    let token = token.filter(|t| valid_token(t))?;
    let data = match data {
        None => Vec::new(),
        Some(encoded) => b64url_decode(&encoded, MAX_REPLY_SEQUENCE_BYTES).ok()?,
    };
    Some(ParsedReply {
        token,
        ack: reply_kind.as_deref() == Some("ack"),
        ok: ok?,
        code,
        data,
    })
}

/// Incremental scanner that extracts OSC 778 frames from a raw byte stream.
///
/// Clients read their controlling tty in raw mode while a query is
/// outstanding; the stream interleaves the reply with unrelated bytes
/// (keystroke echo, other terminal reports). The scanner buffers input,
/// yields each complete `ESC ] 778 ; …` frame body (ST- or BEL-terminated),
/// and discards everything else.
#[derive(Default)]
pub struct ReplyScanner {
    buf: Vec<u8>,
}

impl ReplyScanner {
    /// Appends raw bytes read from the transport.
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
        // Unrelated bytes are dropped eagerly so an adversarial or chatty
        // stream cannot grow the buffer: everything before the last
        // possible frame start is unrecoverable noise once scanned.
        if self.buf.len() > 2 * MAX_REPLY_SEQUENCE_BYTES {
            let excess = self.buf.len() - 2 * MAX_REPLY_SEQUENCE_BYTES;
            self.buf.drain(..excess);
        }
    }

    /// Extracts the next complete OSC 778 frame body, if any.
    pub fn next_frame(&mut self) -> Option<String> {
        const PREFIX: &[u8] = b"\x1b]778;";
        loop {
            let start = self
                .buf
                .windows(PREFIX.len())
                .position(|window| window == PREFIX)?;
            let body_start = start + PREFIX.len();
            let mut end = None;
            let mut next = body_start;
            for i in body_start..self.buf.len() {
                match self.buf[i] {
                    0x07 => {
                        end = Some(i);
                        next = i + 1;
                        break;
                    }
                    0x1b if self.buf.get(i + 1) == Some(&b'\\') => {
                        end = Some(i);
                        next = i + 2;
                        break;
                    }
                    _ => {}
                }
            }
            let Some(end) = end else {
                // Incomplete frame: keep from the frame start, drop noise.
                self.buf.drain(..start);
                return None;
            };
            let body = String::from_utf8_lossy(&self.buf[body_start..end]).into_owned();
            self.buf.drain(..next);
            if !body.is_empty() {
                return Some(body);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b64url_round_trips_rfc4648_vectors() {
        // RFC 4648 §10 vectors, translated to the url-safe alphabet.
        let cases: &[(&[u8], &str)] = &[
            (b"", ""),
            (b"f", "Zg"),
            (b"fo", "Zm8"),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg"),
            (b"fooba", "Zm9vYmE"),
            (b"foobar", "Zm9vYmFy"),
            (&[0xfb, 0xff, 0xfe], "-__-"),
        ];
        for (raw, encoded) in cases {
            assert_eq!(b64url_encode(raw), *encoded);
            assert_eq!(b64url_decode(encoded, 64).as_deref(), Ok(*raw));
        }
    }

    #[test]
    fn b64url_decode_rejects_bad_input() {
        assert_eq!(b64url_decode("a", 64), Err(B64DecodeError::BadLength));
        assert_eq!(b64url_decode("Zg=", 64), Err(B64DecodeError::BadChar));
        assert_eq!(b64url_decode("Zm9v", 2), Err(B64DecodeError::TooLarge));
        assert_eq!(b64url_decode("Zm;v", 64), Err(B64DecodeError::BadChar));
    }

    #[test]
    fn ops_are_printable_ascii_without_field_injection() {
        assert!(valid_op("state.visible_objects"));
        assert!(valid_op("caps"));
        assert!(!valid_op(""));
        assert!(!valid_op("state.scene;t=r"));
        assert!(!valid_op("state scene"));
        assert!(!valid_op("=oops"));
        assert!(!valid_op("op\x07"));
    }

    #[test]
    fn tokens_are_b64url_alphabet_and_bounded() {
        assert!(valid_token("abc-DEF_123"));
        assert!(!valid_token(""));
        assert!(!valid_token("has space"));
        assert!(!valid_token("semi;colon"));
        assert!(!valid_token(&"x".repeat(MAX_TOKEN_BYTES + 1)));
    }

    #[test]
    fn query_sequence_parses_back_through_the_terminal_gate() {
        let sequence = query_sequence("tok123", "state.scene", Some(b"{\"a\":1}"));
        assert!(sequence.starts_with("\x1b]778;"));
        assert!(sequence.ends_with("\x1b\\"));
        // Split the way vt100 does: strip framing, split on ';'.
        let body = &sequence[2..sequence.len() - 2];
        let params: Vec<&[u8]> = body.split(';').map(str::as_bytes).collect();
        assert_eq!(
            parse_778(&params),
            Some(Wire778::Query(QueryEnvelope {
                token: "tok123".into(),
                op: "state.scene".into(),
                data: b"{\"a\":1}".to_vec(),
            }))
        );
    }

    #[test]
    fn queries_without_data_parse() {
        let sequence = query_sequence("t0", "caps", None);
        let body = &sequence[2..sequence.len() - 2];
        let params: Vec<&[u8]> = body.split(';').map(str::as_bytes).collect();
        assert_eq!(
            parse_778(&params),
            Some(Wire778::Query(QueryEnvelope {
                token: "t0".into(),
                op: "caps".into(),
                data: Vec::new(),
            }))
        );
    }

    #[test]
    fn foreign_osc_is_not_ours() {
        assert_eq!(parse_778(&[b"777", b"ratty:mode", b"3d"]), None);
        assert_eq!(parse_778(&[b"52", b"c", b"data"]), None);
    }

    #[test]
    fn reply_echo_is_swallowed() {
        assert_eq!(
            parse_778(&[b"778", b"v=1", b"t=r", b"id=tok", b"ok=1"]),
            Some(Wire778::ReplyEcho)
        );
    }

    #[test]
    fn malformed_queries_keep_a_recoverable_token() {
        // Wrong version, token recoverable.
        assert_eq!(
            parse_778(&[b"778", b"v=9", b"t=q", b"id=tok", b"op=caps"]),
            Some(Wire778::Malformed {
                token: Some("tok".into()),
                code: codes::BAD_VERSION,
            })
        );
        // Missing op.
        assert_eq!(
            parse_778(&[b"778", b"v=1", b"t=q", b"id=tok"]),
            Some(Wire778::Malformed {
                token: Some("tok".into()),
                code: codes::BAD_ENVELOPE,
            })
        );
        // Invalid token: no correlation possible.
        assert_eq!(
            parse_778(&[b"778", b"v=1", b"t=q", b"id=bad token", b"op=caps"]),
            Some(Wire778::Malformed {
                token: None,
                code: codes::BAD_ENVELOPE,
            })
        );
        // Bad payload encoding.
        assert_eq!(
            parse_778(&[b"778", b"v=1", b"t=q", b"id=tok", b"op=caps", b"data=!!"]),
            Some(Wire778::Malformed {
                token: Some("tok".into()),
                code: codes::BAD_PAYLOAD,
            })
        );
    }

    #[test]
    fn oversized_queries_are_refused_at_parse() {
        let huge = "A".repeat(MAX_QUERY_SEQUENCE_BYTES);
        let huge_field = format!("data={huge}");
        let params: Vec<&[u8]> = vec![
            b"778",
            b"v=1",
            b"t=q",
            b"id=tok",
            b"op=caps",
            huge_field.as_bytes(),
        ];
        assert_eq!(
            parse_778(&params),
            Some(Wire778::Malformed {
                token: Some("tok".into()),
                code: codes::TOO_LARGE,
            })
        );
    }

    #[test]
    fn reply_sequence_is_a_single_parseable_line() {
        // Mirrors the RGP support-reply contract: single line, strict
        // ASCII, exact framing, parseable by the client scanner.
        let reply = reply_sequence("tok", false, true, None, Some(b"{\"n\":7}"));
        let text = String::from_utf8(reply.clone()).expect("replies are ASCII");
        assert!(!text.contains('\n'));
        assert!(text.is_ascii());
        assert!(text.starts_with("\x1b]778;"));
        assert!(text.ends_with("\x1b\\"));

        let mut scanner = ReplyScanner::default();
        scanner.push(b"noise before ");
        scanner.push(&reply);
        let body = scanner.next_frame().expect("frame extracted");
        let parsed = parse_reply_body(&body).expect("reply parses");
        assert_eq!(
            parsed,
            ParsedReply {
                token: "tok".into(),
                ack: false,
                ok: true,
                code: None,
                data: b"{\"n\":7}".to_vec(),
            }
        );
    }

    #[test]
    fn ack_and_error_replies_round_trip() {
        let reply = reply_sequence("t1", true, false, Some(codes::ALREADY_EXISTS), None);
        let mut scanner = ReplyScanner::default();
        scanner.push(&reply);
        let parsed = parse_reply_body(&scanner.next_frame().expect("frame")).expect("parses");
        assert!(parsed.ack);
        assert!(!parsed.ok);
        assert_eq!(parsed.code.as_deref(), Some(codes::ALREADY_EXISTS));
        assert!(parsed.data.is_empty());
    }

    #[test]
    fn scanner_survives_split_frames_and_interleaved_noise() {
        let reply = reply_sequence("tok", false, true, None, None);
        let mut scanner = ReplyScanner::default();
        scanner.push(b"key\x1b[Astrokes");
        scanner.push(&reply[..5]);
        assert!(scanner.next_frame().is_none());
        scanner.push(&reply[5..]);
        let body = scanner.next_frame().expect("frame completes across pushes");
        assert!(parse_reply_body(&body).is_some());
        // BEL-terminated frames are accepted too.
        scanner.push(b"\x1b]778;v=1;t=r;id=t2;ok=1\x07");
        let body = scanner.next_frame().expect("BEL frame");
        assert_eq!(parse_reply_body(&body).expect("parses").token, "t2");
    }

    #[test]
    fn scanner_bounds_its_buffer_against_noise_floods() {
        let mut scanner = ReplyScanner::default();
        for _ in 0..100 {
            scanner.push(&[b'x'; 1024]);
        }
        assert!(scanner.buf.len() <= 2 * MAX_REPLY_SEQUENCE_BYTES);
        assert!(scanner.next_frame().is_none());
    }
}
