//! SIP digest authentication for the B2BUA: nonce issue/replay-guard and challenge.

use super::*;

impl SipServer {
    /// How long a challenge nonce stays valid (seconds).
    const NONCE_TTL: i64 = 300;

    /// Issue a fresh nonce, remember it (with a zero nonce-count baseline for the replay guard),
    /// and return it.
    pub(super) fn issue_nonce(&self) -> String {
        // CSPRNG-backed via UUIDv7's random component. (Predictable dialog tags — a separate,
        // lower-severity item — are tracked elsewhere; the nonce here is not guessable in a way
        // that matters because it is also validated against this server-side set.)
        let nonce: String = format!("{}{}", Uuid::now_v7(), Uuid::now_v7())
            .chars()
            .filter(|c| c.is_ascii_hexdigit())
            .take(32)
            .collect();
        let exp = now_unix() + Self::NONCE_TTL;
        let mut g = self.nonces.lock().expect("nonces mutex");
        g.retain(|_, s| s.exp > now_unix());
        g.insert(nonce.clone(), NonceState { exp, highest_nc: 0 });
        nonce
    }

    /// Validate that `nonce` is one we issued and unexpired, and that `nc` (if the client sent a
    /// nonce-count) is strictly greater than the highest we have already accepted for it — the
    /// replay guard. Read-only: it does not advance state (that happens in [`nonce_commit`] only
    /// after the digest response itself verifies, so a wrong-password attempt can't burn state).
    ///
    /// [`nonce_commit`]: Self::nonce_commit
    pub(super) fn nonce_check(&self, nonce: &str, nc: Option<&str>) -> bool {
        let now = now_unix();
        let mut g = self.nonces.lock().expect("nonces mutex");
        g.retain(|_, s| s.exp > now);
        let Some(state) = g.get(nonce) else {
            return false;
        };
        match nc.and_then(parse_nc) {
            Some(n) => n > state.highest_nc,
            // No nc (client didn't use qop): the nonce is single-use — allowed once while present.
            None => true,
        }
    }

    /// Commit a successful authentication against `nonce`: advance the highest accepted `nc`, or,
    /// when the client sent no `nc`, consume the nonce outright (single-use). Called only after
    /// the digest response has verified.
    pub(super) fn nonce_commit(&self, nonce: &str, nc: Option<&str>) {
        let mut g = self.nonces.lock().expect("nonces mutex");
        match nc.and_then(parse_nc) {
            Some(n) => {
                if let Some(state) = g.get_mut(nonce) {
                    state.highest_nc = state.highest_nc.max(n);
                }
            }
            None => {
                g.remove(nonce);
            }
        }
    }

    /// Verify the request's `Authorization` digest for `method` against the stored per-device
    /// secret. On success returns the authenticated **username** so the caller can bind it to the
    /// request's claimed identity (REGISTER AoR / INVITE From); `None` on any failure. Enforces
    /// nonce validity and replay protection.
    pub(super) async fn digest_ok(&self, msg: &SipMessage, method: &str) -> Option<String> {
        let creds = msg
            .header("Authorization")
            .and_then(crate::sip::digest::Credentials::parse)?;
        if !self.nonce_check(&creds.nonce, creds.nc.as_deref()) {
            return None;
        }
        let secret = match self
            .store
            .get_sip_credential(self.default_tenant, &creds.username)
            .await
        {
            Ok(Some(secret)) => secret,
            _ => return None,
        };
        if !crate::sip::digest::verify(&creds, method, &secret) {
            return None;
        }
        // Authentication succeeded — advance/consume the nonce so this exact request can't be
        // replayed within the nonce's lifetime.
        self.nonce_commit(&creds.nonce, creds.nc.as_deref());
        Some(creds.username)
    }

    /// Whether digest auth must be enforced for a request from `src`. Always enforced when
    /// `require_auth` is configured; additionally enforced (regardless of the flag) for any
    /// **untrusted (public) source address**, so an internet-exposed SIP port never accepts an
    /// unauthenticated REGISTER/INVITE even in a zero-config deployment.
    pub(super) fn auth_required_from(&self, src: SocketAddr) -> bool {
        self.require_auth || !crate::net::is_trusted_ip(&src.ip())
    }

    /// Send a `401 Unauthorized` with a fresh digest challenge, prompting the phone to
    /// re-send the request with an `Authorization` header.
    pub(super) async fn send_challenge(
        &self,
        resp: &Responder,
        msg: &SipMessage,
    ) -> std::io::Result<()> {
        let challenge = crate::sip::digest::Challenge::new(self.realm.clone(), self.issue_nonce());
        let reply = message::response_with(
            msg,
            401,
            "Unauthorized",
            &[("WWW-Authenticate", challenge.header_value())],
        );
        resp.send(reply.as_bytes()).await
    }
}
