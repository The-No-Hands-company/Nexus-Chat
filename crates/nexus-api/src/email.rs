//! Email delivery via Resend API.
//!
//! When `NEXUS__EMAIL__API_KEY` is set, emails are sent through the Resend
//! REST API (https://resend.com/docs/api-reference/emails/send-email).
//! When unset, email sending is a no-op and verification tokens are logged
//! at DEBUG level for development convenience.

use nexus_common::config::EmailConfig;
use serde::Serialize;

/// Lightweight email sender backed by Resend's HTTP API.
#[derive(Clone)]
pub struct EmailService {
    client: reqwest::Client,
    config: EmailConfig,
    endpoint: String,
}

#[derive(Serialize)]
struct ResendPayload {
    from: String,
    to: Vec<String>,
    subject: String,
    html: String,
}

impl EmailService {
    pub fn new(config: EmailConfig) -> Self {
        Self {
            client: reqwest::Client::new(),
            config,
            endpoint: "https://api.resend.com/emails".to_owned(),
        }
    }

    /// Returns `true` when a valid Resend API key is configured.
    pub fn is_enabled(&self) -> bool {
        self.config.is_enabled()
    }

    /// Send an email via Resend. Returns `Ok(())` on success, or an error string.
    /// If email is not configured, this is a no-op that returns `Ok(())`.
    async fn send(&self, to: &str, subject: &str, html: &str) -> Result<(), String> {
        if !self.is_enabled() {
            tracing::debug!("Email not sent (no API key configured)");
            return Ok(());
        }

        let payload = ResendPayload {
            from: self.config.from.clone(),
            to: vec![to.to_owned()],
            subject: subject.to_owned(),
            html: html.to_owned(),
        };

        let resp = self
            .client
            .post(&self.endpoint)
            .header("Authorization", format!("Bearer {}", self.config.api_key))
            .json(&payload)
            .send()
            .await
            .map_err(|e| {
                // reqwest errors can embed the request URL; use a fixed label only
                let kind = if e.is_timeout() { "timeout" } else if e.is_connect() { "connect" } else { "request" };
                format!("Failed to send email ({kind})")
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
                        tracing::error!(status = %status, "Resend API error");
            return Err(format!("Resend API returned {status}"));
        }

        tracing::info!("Email sent via Resend");
        Ok(())
    }

    /// Send a verification email with a clickable link.
    ///
    /// The `raw_token` is the unhashed token stored in the database row.
    /// `base_url` is read from `EmailConfig.base_url`.
    pub async fn send_verification_email(
        &self,
        to_email: &str,
        username: &str,
        raw_token: &str,
    ) -> Result<(), String> {
        if !self.is_enabled() {
            tracing::debug!("Verification email not sent (no API key configured)");
            return Ok(());
        }

        let base = self.config.base_url.trim_end_matches('/');
        let verify_url = format!("{base}/api/v1/auth/verify-email?token={raw_token}");

        let html = format!(
            r#"<!DOCTYPE html>
<html>
<head><meta charset="utf-8"></head>
<body style="font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif; background: #1a1a2e; color: #e0e0e0; padding: 40px;">
  <div style="max-width: 480px; margin: 0 auto; background: #16213e; border-radius: 12px; padding: 32px; text-align: center;">
    <div style="width: 56px; height: 56px; border-radius: 14px; background: #7c3aed; display: inline-flex; align-items: center; justify-content: center; margin-bottom: 16px;">
      <span style="color: white; font-size: 28px; font-weight: bold;">N</span>
    </div>
    <h1 style="margin: 0 0 8px; font-size: 22px; color: #fff;">Verify your email</h1>
    <p style="color: #a0a0b0; font-size: 14px; margin: 0 0 24px;">
      Hey <strong style="color:#fff;">{username}</strong>, click the button below to verify your email address and unlock full access to Nexus.
    </p>
    <a href="{verify_url}" style="display: inline-block; background: #7c3aed; color: white; text-decoration: none; padding: 12px 32px; border-radius: 8px; font-weight: 600; font-size: 15px;">
      Verify Email
    </a>
    <p style="color: #6b6b7b; font-size: 12px; margin: 24px 0 0;">
      If you didn't create a Nexus account, you can safely ignore this email.<br>
      This link expires in 24 hours.
    </p>
  </div>
</body>
</html>"#
        );

        self.send(to_email, "Verify your Nexus email", &html).await
    }

    /// Send a password reset email with a clickable one-time link.
    pub async fn send_password_reset_email(
        &self,
        to_email: &str,
        username: &str,
        raw_token: &str,
    ) -> Result<(), String> {
        if !self.is_enabled() {
            tracing::debug!("Password reset email not sent (no API key configured)");
            return Ok(());
        }

        let base = self.config.base_url.trim_end_matches('/');
        let reset_url = format!("{base}/reset-password?token={raw_token}");

        let html = format!(
            r#"<!DOCTYPE html>
<html>
<head><meta charset=\"utf-8\"></head>
<body style=\"font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif; background: #1a1a2e; color: #e0e0e0; padding: 40px;\">
    <div style=\"max-width: 480px; margin: 0 auto; background: #16213e; border-radius: 12px; padding: 32px; text-align: center;\">
        <h1 style=\"margin: 0 0 8px; font-size: 22px; color: #fff;\">Reset your password</h1>
        <p style=\"color: #a0a0b0; font-size: 14px; margin: 0 0 24px;\">
            Hey <strong style=\"color:#fff;\">{username}</strong>, click below to reset your Nexus password.
            This link expires in 2 hours and can only be used once.
        </p>
        <a href=\"{reset_url}\" style=\"display: inline-block; background: #7c3aed; color: white; text-decoration: none; padding: 12px 32px; border-radius: 8px; font-weight: 600; font-size: 15px;\">
            Reset Password
        </a>
        <p style=\"color: #6b6b7b; font-size: 12px; margin: 24px 0 0;\">
            If you didn't request this, you can safely ignore this email.
        </p>
    </div>
</body>
</html>"#
        );

        self.send(to_email, "Reset your Nexus password", &html)
            .await
    }
}

#[cfg(test)]
mod tests {
    //! Zero-retention: neither the recipient address nor the token may reach a log line.
    use super::*;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);
    impl Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Buf {
        type Writer = Buf;
        fn make_writer(&'a self) -> Buf {
            self.clone()
        }
    }

    const ADDR: &str = "private.person@example.org";
    const TOKEN: &str = "tok_marker_value_for_test";

    fn capture() -> (Buf, tracing::subscriber::DefaultGuard) {
        let buf = Buf::default();
        let sub = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(buf.clone())
            .finish();
        (buf, tracing::subscriber::set_default(sub))
    }

    fn text(b: &Buf) -> String {
        String::from_utf8(b.0.lock().unwrap().clone()).unwrap()
    }

    fn cfg(key: &str) -> EmailConfig {
        EmailConfig {
            api_key: key.into(),
            from: "Nexus <noreply@example.org>".into(),
            base_url: "https://example.org".into(),
        }
    }

    fn assert_clean(out: &str) {
        assert!(!out.contains(ADDR), "recipient leaked: {out}");
        assert!(!out.contains("private.person"), "recipient leaked: {out}");
        assert!(!out.contains(TOKEN), "token leaked: {out}");
        assert!(!out.contains("Verify your Nexus email"), "subject leaked: {out}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn no_api_key_path_logs_neither_address_nor_token() {
        let (buf, _g) = capture();
        let svc = EmailService::new(cfg(""));
        svc.send_verification_email(ADDR, "someone", TOKEN).await.unwrap();
        svc.send_password_reset_email(ADDR, "someone", TOKEN).await.unwrap();
        let out = text(&buf);
        assert!(out.contains("no API key configured"), "expected a log line: {out}");
        assert_clean(&out);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn resend_error_response_logs_status_only() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut tmp = [0u8; 4096];
            let _ = sock.read(&mut tmp).await;
            let body = format!("{{\"message\":\"invalid recipient {ADDR} token {TOKEN}\"}}");
            let resp = format!(
                "HTTP/1.1 422 Unprocessable Entity\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        });
        let (buf, _g) = capture();
        let mut svc = EmailService::new(cfg("re_test"));
        svc.endpoint = format!("http://127.0.0.1:{port}/emails");
        let r = svc.send_verification_email(ADDR, "someone", TOKEN).await;
        assert!(r.is_err());
        let out = text(&buf);
        assert!(out.contains("Resend API error"), "expected error log: {out}");
        assert_clean(&out);
        assert!(!r.unwrap_err().contains(ADDR));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn connect_failure_logs_and_returns_no_address() {
        let (buf, _g) = capture();
        let mut svc = EmailService::new(cfg("re_test"));
        svc.endpoint = "http://127.0.0.1:1/emails".into();
        let r = svc.send_verification_email(ADDR, "someone", TOKEN).await;
        assert!(r.is_err());
        assert_clean(&text(&buf));
        assert_clean(&r.unwrap_err());
    }
}
