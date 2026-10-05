//! Emails: queued in the `email` table (in the transaction that needs them),
//! sent by the `email_send` job. A link's token is created at send time, in
//! the send's transaction: a failed send rolls it back, the retry makes a new
//! one, and the queue never holds a token (docs/decisions.md, Background jobs).

use std::time::Duration;

use askama::Template;
use lettre::message::{Mailbox, MultiPart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::api::AppState;
use crate::db;
use crate::jobs::JobError;
use crate::settings::{Settings, Smtp, SmtpTls};

/// How long a link works.
pub const VERIFICATION_LIFETIME: Duration = Duration::from_secs(7 * 24 * 3600);
pub const RESET_LIFETIME: Duration = Duration::from_secs(3600);
pub const INVITATION_LIFETIME: Duration = Duration::from_secs(14 * 24 * 3600);

/// Delays before each retry; given up after the last one.
const RETRIES: [u64; 5] = [60, 300, 1800, 7200, 43200];

/// An email to send, with what its template needs (stored as `params`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "template", rename_all = "snake_case")]
pub enum Email {
    /// Confirms a self-registered account's address.
    Verification { account: Uuid },
    /// Someone tried to register with an address that has an account.
    RegistrationAttempt,
    /// A link to choose a new password.
    PasswordReset { account: Uuid },
    /// Joins an organization, creating the account (the invited user).
    Invitation {
        user: Uuid,
        organization: String,
        inviter: String,
    },
    /// An existing account was added to an organization.
    MemberAdded {
        organization: String,
        inviter: String,
    },
}

impl Email {
    fn name(&self) -> &'static str {
        match self {
            Email::Verification { .. } => "verification",
            Email::RegistrationAttempt => "registration_attempt",
            Email::PasswordReset { .. } => "password_reset",
            Email::Invitation { .. } => "invitation",
            Email::MemberAdded { .. } => "member_added",
        }
    }

    fn subject(&self, instance: &str) -> String {
        match self {
            Email::Verification { .. } => format!("Confirm your email address for {instance}"),
            Email::RegistrationAttempt => {
                format!("Someone tried to sign up on {instance} with your address")
            }
            Email::PasswordReset { .. } => format!("Choose a new password for {instance}"),
            Email::Invitation { organization, .. } => format!("Join {organization} on {instance}"),
            Email::MemberAdded { organization, .. } => {
                format!("You joined {organization} on {instance}")
            }
        }
    }

    /// The token its link carries: `(usage, resource, web app page)`.
    fn token(&self) -> Option<(&'static str, Uuid, &'static str)> {
        match self {
            Email::Verification { account } => Some(("email_verification", *account, "verify")),
            Email::PasswordReset { account } => Some(("password_reset", *account, "reset")),
            Email::Invitation { user, .. } => Some(("invitation", *user, "invite")),
            Email::RegistrationAttempt | Email::MemberAdded { .. } => None,
        }
    }

    /// `(text, html)`.
    fn render(&self, context: &Context) -> askama::Result<(String, String)> {
        let e = context;
        Ok(match self {
            Email::Verification { .. } => (
                VerificationText { e }.render()?,
                VerificationHtml { e }.render()?,
            ),
            Email::RegistrationAttempt => (
                RegistrationAttemptText { e }.render()?,
                RegistrationAttemptHtml { e }.render()?,
            ),
            Email::PasswordReset { .. } => (
                PasswordResetText { e }.render()?,
                PasswordResetHtml { e }.render()?,
            ),
            Email::Invitation { .. } => (
                InvitationText { e }.render()?,
                InvitationHtml { e }.render()?,
            ),
            Email::MemberAdded { .. } => (
                MemberAddedText { e }.render()?,
                MemberAddedHtml { e }.render()?,
            ),
        })
    }
}

/// What the templates see (fields an email doesn't use are empty).
struct Context {
    instance_name: String,
    public_url: String,
    link: String,
    organization: String,
    inviter: String,
}

macro_rules! templates {
    ($($text:ident $html:ident $file:literal $file_html:literal),* $(,)?) => {$(
        #[derive(Template)]
        #[template(path = $file)]
        struct $text<'a> {
            e: &'a Context,
        }
        #[derive(Template)]
        #[template(path = $file_html)]
        struct $html<'a> {
            e: &'a Context,
        }
    )*};
}

templates!(
    VerificationText VerificationHtml "verification.en.txt" "verification.en.html",
    RegistrationAttemptText RegistrationAttemptHtml "registration_attempt.en.txt" "registration_attempt.en.html",
    PasswordResetText PasswordResetHtml "password_reset.en.txt" "password_reset.en.html",
    InvitationText InvitationHtml "invitation.en.txt" "invitation.en.html",
    MemberAddedText MemberAddedHtml "member_added.en.txt" "member_added.en.html",
);

/// Queues an email, in the caller's transaction.
pub async fn enqueue(db: &mut PgConnection, recipient: &str, email: &Email) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO email (recipient, template, params) VALUES ($1, $2, $3)",
        recipient,
        email.name(),
        serde_json::to_value(email).expect("serializable")
    )
    .execute(db)
    .await?;
    Ok(())
}

/// The `email_send` job: sends the due emails, oldest first. Reads the SMTP
/// settings from the database at each run, so changes apply without a restart.
pub async fn send_due(state: &AppState) -> Result<i64, JobError> {
    let settings = state.settings.get();
    let Some(smtp) = &settings.smtp else {
        return Ok(0); // no SMTP: emails stay queued
    };
    let mailer = mailer(smtp).map_err(JobError::Other)?;
    let mut sent = 0;
    loop {
        let due = sqlx::query!(
            "SELECT pk, recipient, params, attempts FROM email
             WHERE sent_at IS NULL AND given_up_at IS NULL AND next_attempt_at <= now()
             ORDER BY next_attempt_at LIMIT 100"
        )
        .fetch_all(&state.pool)
        .await?;
        if due.is_empty() {
            return Ok(sent);
        }
        for row in due {
            match send(
                state,
                &settings,
                smtp,
                &mailer,
                row.pk,
                &row.recipient,
                row.params,
            )
            .await
            {
                Ok(()) => {
                    sent += 1;
                    metrics::counter!("theca_emails_total", "outcome" => "sent").increment(1);
                }
                Err(Failure::Retry(error)) => {
                    failed(&state.pool, row.pk, row.attempts, &error, false).await?
                }
                Err(Failure::GiveUp(error)) => {
                    failed(&state.pool, row.pk, row.attempts, &error, true).await?
                }
                Err(Failure::Database(e)) => return Err(e.into()),
            }
        }
    }
}

enum Failure {
    /// The SMTP server refused or couldn't be reached: later.
    Retry(String),
    /// Never sendable (bad address, broken template).
    GiveUp(String),
    Database(sqlx::Error),
}

impl From<sqlx::Error> for Failure {
    fn from(e: sqlx::Error) -> Self {
        Failure::Database(e)
    }
}

/// Sends one email: its token (if any) and `sent_at` in one transaction with the send.
async fn send(
    state: &AppState,
    settings: &Settings,
    smtp: &Smtp,
    mailer: &AsyncSmtpTransport<Tokio1Executor>,
    pk: i64,
    recipient: &str,
    params: serde_json::Value,
) -> Result<(), Failure> {
    let email: Email =
        serde_json::from_value(params).map_err(|e| Failure::GiveUp(format!("params: {e}")))?;
    let public_url = state.signer.public_url().to_owned();
    let mut tx = state.pool.begin().await?;
    let link = match email.token() {
        Some((usage, resource, page)) => {
            let token = db::account::create_one_off(&mut tx, usage, resource).await?;
            format!("{public_url}/{page}?token={}", token.token)
        }
        None => public_url.clone(),
    };
    let (organization, inviter) = match &email {
        Email::Invitation {
            organization,
            inviter,
            ..
        }
        | Email::MemberAdded {
            organization,
            inviter,
        } => (organization.clone(), inviter.clone()),
        _ => Default::default(),
    };
    let context = Context {
        instance_name: settings.instance_name.clone(),
        public_url,
        link,
        organization,
        inviter,
    };
    let (text, html) = email
        .render(&context)
        .map_err(|e| Failure::GiveUp(format!("template: {e}")))?;
    let to: Mailbox = recipient
        .parse()
        .map_err(|e| Failure::GiveUp(format!("recipient: {e}")))?;
    let from: Mailbox = smtp
        .from
        .parse()
        .map_err(|e| Failure::GiveUp(format!("smtp_from: {e}")))?;
    let message = Message::builder()
        .from(from)
        .to(to)
        .subject(email.subject(&settings.instance_name))
        .multipart(MultiPart::alternative_plain_html(text, html))
        .map_err(|e| Failure::GiveUp(format!("message: {e}")))?;
    mailer
        .send(message)
        .await
        .map_err(|e| Failure::Retry(e.to_string()))?;
    sqlx::query!(
        "UPDATE email SET sent_at = now(), attempts = attempts + 1 WHERE pk = $1",
        pk
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Records a failed attempt: retried after a growing delay, or given up.
async fn failed(
    pool: &PgPool,
    pk: i64,
    attempts: i32,
    error: &str,
    give_up: bool,
) -> sqlx::Result<()> {
    let delay = RETRIES.get(attempts as usize).filter(|_| !give_up);
    let outcome = if delay.is_some() {
        "retried"
    } else {
        "given_up"
    };
    metrics::counter!("theca_emails_total", "outcome" => outcome).increment(1);
    tracing::warn!(
        email = pk,
        attempts = attempts + 1,
        error,
        given_up = delay.is_none(),
        "email not sent"
    );
    sqlx::query!(
        "UPDATE email SET attempts = attempts + 1, last_error = $2,
             next_attempt_at = now() + make_interval(secs => coalesce($3::float8, 0)),
             given_up_at = CASE WHEN $3::float8 IS NULL THEN now() END
         WHERE pk = $1",
        pk,
        error,
        delay.map(|d| *d as f64)
    )
    .execute(pool)
    .await?;
    Ok(())
}

fn mailer(smtp: &Smtp) -> Result<AsyncSmtpTransport<Tokio1Executor>, String> {
    let builder = match smtp.tls {
        SmtpTls::StartTls => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&smtp.host),
        SmtpTls::Tls => AsyncSmtpTransport::<Tokio1Executor>::relay(&smtp.host),
        SmtpTls::None => Ok(AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(
            &smtp.host,
        )),
    }
    .map_err(|e| format!("SMTP: {e}"))?;
    let mut builder = builder
        .port(smtp.port)
        .timeout(Some(Duration::from_secs(30)));
    if let Some(username) = &smtp.username {
        let password = match &smtp.password_file {
            Some(path) => std::fs::read_to_string(path)
                .map_err(|e| format!("smtp_password_file ({path}): {e}"))?
                .trim()
                .to_owned(),
            None => String::new(),
        };
        builder = builder.credentials(Credentials::new(username.clone(), password));
    }
    Ok(builder.build())
}
