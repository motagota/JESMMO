//! Account authentication: password hashing and the register/login flows.
//!
//! Passwords are hashed with Argon2 (PHC string stored in `account.pw_hash`);
//! plaintext is never persisted. Session *tokens* are minted and tracked by the
//! gateway in memory (see proxy), so this module only owns the durable identity
//! steps that hit the database.

use argon2::password_hash::{rand_core::OsRng, PasswordHash, SaltString};
use argon2::{Argon2, PasswordHasher, PasswordVerifier};

use crate::persistence::{Character, Db, DbError};

/// Why an auth attempt failed. `message()` is safe to send to the client.
#[derive(Debug)]
pub enum AuthError {
    EmailTaken,
    InvalidCredentials,
    BadRequest(String),
    Db(DbError),
    Hash(String),
}

impl AuthError {
    pub fn message(&self) -> String {
        match self {
            AuthError::EmailTaken => "that email is already registered".to_string(),
            AuthError::InvalidCredentials => "invalid email or password".to_string(),
            AuthError::BadRequest(m) => m.clone(),
            // Don't leak internals to the client; log the detail server-side instead.
            AuthError::Db(_) => "server error, please try again".to_string(),
            AuthError::Hash(_) => "server error, please try again".to_string(),
        }
    }
}

/// Hash a password into an Argon2 PHC string suitable for storage.
pub fn hash_password(password: &str) -> Result<String, AuthError> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| AuthError::Hash(e.to_string()))
}

/// Constant-time-ish verification of a password against a stored PHC hash.
pub fn verify_password(password: &str, phc_hash: &str) -> bool {
    match PasswordHash::new(phc_hash) {
        Ok(parsed) => Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

/// Register a new account + its starter character at the given spawn position.
pub async fn register(
    db: &Db,
    email: &str,
    password: &str,
    name: &str,
    spawn_x: i64,
    spawn_y: i64,
    spawn_hp: i64,
) -> Result<Character, AuthError> {
    let email = email.trim();
    if email.is_empty() || !email.contains('@') {
        return Err(AuthError::BadRequest("a valid email is required".to_string()));
    }
    if password.len() < 4 {
        return Err(AuthError::BadRequest(
            "password must be at least 4 characters".to_string(),
        ));
    }
    if db.find_account_by_email(email).await.map_err(AuthError::Db)?.is_some() {
        return Err(AuthError::EmailTaken);
    }

    let display_name = if name.trim().is_empty() { email } else { name.trim() };
    let hash = hash_password(password)?;
    let (_account, character) = db
        .create_account_with_character(email, &hash, display_name, spawn_x, spawn_y, spawn_hp)
        .await
        .map_err(AuthError::Db)?;
    Ok(character)
}

/// Verify credentials and return the account's character (with its saved state).
pub async fn login(db: &Db, email: &str, password: &str) -> Result<Character, AuthError> {
    let email = email.trim();
    let account = db
        .find_account_by_email(email)
        .await
        .map_err(AuthError::Db)?
        .ok_or(AuthError::InvalidCredentials)?;

    if !verify_password(password, &account.pw_hash) {
        return Err(AuthError::InvalidCredentials);
    }

    db.touch_login(&account.id).await.map_err(AuthError::Db)?;
    db.character_for_account(&account.id)
        .await
        .map_err(AuthError::Db)?
        .ok_or(AuthError::InvalidCredentials)
}
