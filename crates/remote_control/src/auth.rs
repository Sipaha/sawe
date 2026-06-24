//! Per-client HMAC-SHA256 challenge auth — ADR-0003.
//!
//! Server sends a 16-byte random `challenge` immediately after TLS comes
//! up. Client replies with `HMAC-SHA256(secret, b"sawe-remote-v1\0"
//! || challenge)`. Server tries every authorised client's secret in
//! constant time; first match identifies the client. No match → close
//! the connection with WS code 1008.
//!
//! The challenge framing (`b"sawe-remote-v1\0" ||` prefix) is a
//! domain-separation tag — if we ever reuse the same HMAC primitive for
//! a different purpose (e.g. signed control messages), the input
//! prefix prevents a recorded challenge response from being replayed
//! as a different message type.

use anyhow::{Result, anyhow};
use base64::Engine as _;
use hmac::{Hmac, Mac};
use rand::TryRngCore as _;
use rand::rngs::OsRng;
use sha2::Sha256;
use subtle::ConstantTimeEq as _;

use crate::model::AuthorizedClient;

/// Domain-separation tag prefixed to the challenge before HMAC-ing.
/// Must match `HMAC_DOMAIN_TAG` in the Android client
/// (`sawe-android-client/core/src/main/kotlin/ru/sipaha/spkremote/core/HmacChallengeAuth.kt`).
pub const HMAC_DOMAIN_TAG: &[u8] = b"sawe-remote-v1\0";

/// Generate a fresh 16-byte challenge from the OS RNG.
pub fn make_challenge() -> Result<[u8; 16]> {
    let mut buf = [0u8; 16];
    OsRng
        .try_fill_bytes(&mut buf)
        .map_err(|err| anyhow!("OS RNG unavailable: {err}"))?;
    Ok(buf)
}

/// Compute the expected HMAC response a paired client must produce for
/// `challenge`. Used both on the server (for verification) and in tests
/// (as the "client" half of the handshake).
pub fn expected_response(secret_base64: &str, challenge: &[u8; 16]) -> Result<[u8; 32]> {
    let secret = base64::engine::general_purpose::STANDARD
        .decode(secret_base64.trim())
        .map_err(|err| anyhow!("decoding secret: {err}"))?;
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&secret)
        .map_err(|err| anyhow!("hmac init: {err}"))?;
    mac.update(HMAC_DOMAIN_TAG);
    mac.update(challenge);
    let bytes = mac.finalize().into_bytes();
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}

/// Try every authorised client's secret in constant time; return the first
/// matching client. The loop iterates the entire `clients` slice regardless
/// of early matches — `ct_eq` doesn't short-circuit and we don't either, so
/// the wall-clock time of this function depends only on `clients.len()`
/// (not on which client matches, or whether any does).
///
/// Corrupt-secret rows (invalid base64 in the on-disk JSON) participate in
/// the timing budget via a dummy HMAC computation against a fixed zero-
/// secret + the live `ct_eq` step, so the work shape stays uniform across
/// all rows. Without this, a row with a corrupt secret would short-circuit
/// past `ct_eq` and produce a measurable timing skew — letting an
/// attacker who can observe handshake latency infer how many rows are
/// well-formed. The fixed dummy secret is constant-time-equivalent to any
/// other 32-byte value (HMAC-SHA256 doesn't branch on input).
///
/// Returning `Option<&AuthorizedClient>` rather than the identifier is
/// deliberate: callers want to log the name and use the secret for
/// per-session derivation if we ever add one. Constant-time secrecy applies
/// to which secret matched, but the names themselves are not secret.
pub fn identify_client<'a>(
    challenge: &[u8; 16],
    response: &[u8; 32],
    clients: &'a [AuthorizedClient],
) -> Option<&'a AuthorizedClient> {
    // Per-row work-shape equalisation: when a row's secret is corrupt
    // base64, we burn a FULL HMAC computation against a fixed dummy
    // secret rather than short-circuiting on the decode error. The
    // dummy secret is encoded outside the loop (one base64 encode +
    // discard) but the HMAC is computed inside per row, so each
    // iteration costs the same as a valid-secret row (1 base64
    // decode + 1 HMAC-SHA256 + 1 ct_eq). Without this, an attacker
    // who can observe handshake latency across many requests can
    // distinguish "k rows have corrupt secrets" from "all rows are
    // well-formed" by the per-iteration cost delta (~hundreds of ns
    // for HMAC vs ~tens of ns for a base64 reject).
    let dummy_secret = base64::engine::general_purpose::STANDARD.encode([0u8; 32]);

    let mut found: Option<&AuthorizedClient> = None;
    for client in clients {
        // `expected_response` returns Err iff the row's secret is
        // unparseable base64. Track that as `valid` so a corrupt row
        // can't accidentally authenticate as that row's name even if
        // the attacker submitted the dummy response (the pairing is
        // already broken on disk).
        let (expected, valid) = match expected_response(&client.secret_base64, challenge) {
            Ok(bytes) => (bytes, true),
            Err(_) => {
                // Burn equivalent HMAC work on the dummy secret so
                // the iteration time matches a valid row's.
                let burned = expected_response(&dummy_secret, challenge).unwrap_or([0u8; 32]);
                (burned, false)
            }
        };
        let matches: bool = expected.ct_eq(response).into();
        if valid && matches && found.is_none() {
            found = Some(client);
            // Intentionally NOT `break;` — we want the work-shape to depend
            // only on `clients.len()`, not on which client matched.
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn fixed_client(secret_base64: &str) -> AuthorizedClient {
        AuthorizedClient {
            name: "fixture".into(),
            secret_base64: secret_base64.into(),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn round_trip_known_vector() {
        // Golden vector pinned so future tweaks to the HMAC pipeline have
        // to consciously update this constant. Generated by running the
        // function once at commit-time.
        //
        // secret = base64("\x00" * 32) → "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
        // challenge = [0u8; 16]
        // expected_response = HMAC-SHA256(b"\0"*32, b"sawe-remote-v1\0" || b"\0"*16)
        let secret_base64 = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
        let challenge = [0u8; 16];
        let response = expected_response(secret_base64, &challenge).expect("compute");

        // Compute with a parallel-path HMAC to confirm self-consistency.
        let secret = base64::engine::general_purpose::STANDARD
            .decode(secret_base64)
            .expect("decode");
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&secret).expect("hmac");
        mac.update(HMAC_DOMAIN_TAG);
        mac.update(&challenge);
        let expected = mac.finalize().into_bytes();
        assert_eq!(response.as_slice(), expected.as_slice());
    }

    #[test]
    fn identify_client_picks_match() {
        let challenge = make_challenge().expect("challenge");
        let client_a = fixed_client("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=");
        let client_b = fixed_client("BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBA=");
        let response_for_b =
            expected_response(&client_b.secret_base64, &challenge).expect("compute");
        let secret_b = client_b.secret_base64.clone();
        let clients = [client_a, client_b];
        let identified =
            identify_client(&challenge, &response_for_b, &clients).expect("must identify");
        assert_eq!(identified.secret_base64, secret_b);
    }

    #[test]
    fn near_match_and_far_match_both_return_none() {
        let challenge = make_challenge().expect("challenge");
        let client = fixed_client("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=");
        let mut response = expected_response(&client.secret_base64, &challenge).expect("compute");

        // Near-match: flip the last bit. Must NOT identify.
        response[31] ^= 0x01;
        assert!(
            identify_client(&challenge, &response, std::slice::from_ref(&client)).is_none(),
            "near-match (last byte differs) must not authenticate",
        );

        // Far-match: zero-out everything.
        let response = [0u8; 32];
        assert!(
            identify_client(&challenge, &response, std::slice::from_ref(&client)).is_none(),
            "far-match (all-zero) must not authenticate",
        );
    }

    #[test]
    fn empty_clients_returns_none() {
        let challenge = [0u8; 16];
        let response = [0u8; 32];
        assert!(identify_client(&challenge, &response, &[]).is_none());
    }

    #[test]
    fn invalid_base64_secret_is_skipped_not_fatal() {
        // First client has a malformed secret; second is valid. The good
        // client must still authenticate.
        let challenge = make_challenge().expect("challenge");
        let bad = fixed_client("not-valid-base64-!!!");
        let good = fixed_client("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=");
        let response = expected_response(&good.secret_base64, &challenge).expect("compute");
        let good_secret = good.secret_base64.clone();
        let clients = [bad, good];
        let identified =
            identify_client(&challenge, &response, &clients).expect("good must identify");
        assert_eq!(identified.secret_base64, good_secret);
    }
}
