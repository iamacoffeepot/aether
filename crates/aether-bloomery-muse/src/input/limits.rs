//! The bounded scalar fields of a turn input: where the request goes, which
//! model answers, how much it may write, and how hard it reasons.
//!
//! Each validated newtype runs its one `check` from both `new` and decode, so
//! a stored input that breaks a rule refuses when the journal hands it back.

use core::borrow::Borrow;

/// Why [`Endpoint::new`] or decode refused a URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointError {
    /// Longer than [`Endpoint::MAX_BYTES`].
    TooLong,
    /// Carried a whitespace or control character.
    BadChar,
    /// Did not start with `https://` or `http://`.
    NotHttp,
    /// Nothing named a host after the scheme.
    NoHost,
}

impl EndpointError {
    const fn reason(self) -> &'static str {
        match self {
            Self::TooLong => "too-long",
            Self::BadChar => "bad-char",
            Self::NotHttp => "not-http",
            Self::NoHost => "no-host",
        }
    }
}

/// The absolute `https://` or `http://` URL a turn posts to.
///
/// Where a request may go is still the HTTP capability's allowlist; this
/// type only refuses what is not a URL a fetch could name.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[storage(validate)]
pub struct Endpoint(String);

impl Endpoint {
    /// Longest accepted URL in bytes.
    pub const MAX_BYTES: usize = 2048;

    /// Accept an absolute HTTP URL.
    ///
    /// # Errors
    ///
    /// The [`EndpointError`] naming the rule the URL broke.
    pub fn new(url: impl Into<String>) -> Result<Self, EndpointError> {
        let url = url.into();
        Self::check(&url)?;
        Ok(Self(url))
    }

    /// Borrow the URL.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn check(url: &str) -> Result<(), EndpointError> {
        if url.len() > Self::MAX_BYTES {
            return Err(EndpointError::TooLong);
        }
        if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(EndpointError::BadChar);
        }
        let rest =
            url.strip_prefix("https://").or_else(|| url.strip_prefix("http://")).ok_or(EndpointError::NotHttp)?;
        if rest.is_empty() || rest.starts_with('/') {
            return Err(EndpointError::NoHost);
        }
        Ok(())
    }
}

/// Why [`ModelName::new`] or decode refused a model name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelNameError {
    /// The name was empty.
    Empty,
    /// Longer than [`ModelName::MAX_BYTES`].
    TooLong,
    /// The first byte was not `[a-z0-9]`.
    BadStart,
    /// A later byte was not `[a-z0-9._-]`.
    BadChar,
}

impl ModelNameError {
    const fn reason(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::TooLong => "too-long",
            Self::BadStart => "bad-start",
            Self::BadChar => "bad-char",
        }
    }
}

/// The model that answers a turn: 1 to 128 bytes matching `[a-z0-9][a-z0-9._-]*`.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[storage(validate)]
pub struct ModelName(String);

impl ModelName {
    /// Longest accepted name in bytes.
    pub const MAX_BYTES: usize = 128;

    /// Accept a model name.
    ///
    /// # Errors
    ///
    /// The [`ModelNameError`] naming the rule the name broke.
    pub fn new(name: impl Into<String>) -> Result<Self, ModelNameError> {
        let name = name.into();
        Self::check(&name)?;
        Ok(Self(name))
    }

    /// Borrow the name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn check(name: &str) -> Result<(), ModelNameError> {
        let Some((&first, rest)) = name.as_bytes().split_first() else {
            return Err(ModelNameError::Empty);
        };
        if name.len() > Self::MAX_BYTES {
            return Err(ModelNameError::TooLong);
        }
        if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
            return Err(ModelNameError::BadStart);
        }
        if !rest.iter().all(|&byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)) {
            return Err(ModelNameError::BadChar);
        }
        Ok(())
    }
}

/// Why [`OutputBudget::new`] or decode refused a token budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputBudgetError {
    /// A budget of zero output tokens.
    Zero,
}

impl OutputBudgetError {
    const fn reason(self) -> &'static str {
        match self {
            Self::Zero => "zero",
        }
    }
}

/// The most output tokens, reasoning included, one turn may produce. Never zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, aether_data::Storage)]
#[storage(validate)]
pub struct OutputBudget(u32);

impl OutputBudget {
    /// Accept a non-zero budget.
    ///
    /// # Errors
    ///
    /// [`OutputBudgetError::Zero`] for a budget of zero.
    pub fn new(tokens: u32) -> Result<Self, OutputBudgetError> {
        Self::check(tokens)?;
        Ok(Self(tokens))
    }

    /// The budget in tokens.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    // `#[storage(validate)]` calls `check(&inner)`; `Borrow` takes that
    // reference and `new`'s owned value alike.
    fn check(tokens: impl Borrow<u32>) -> Result<(), OutputBudgetError> {
        if *tokens.borrow() == 0 {
            Err(OutputBudgetError::Zero)
        } else {
            Ok(())
        }
    }
}

/// How much reasoning the model spends before it answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, aether_data::Storage)]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
}

invariant_errors!(EndpointError, ModelNameError, OutputBudgetError);
