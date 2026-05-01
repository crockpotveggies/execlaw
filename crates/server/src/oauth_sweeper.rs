//! Background sweeper that:
//!   1. Refreshes any access_token within the proactive_refresh_window
//!      (default 10 min before expiry) by calling the provider's
//!      refresh-token grant.
//!   2. Purges expired entries from `state_oauth_pending` (the CSRF
//!      window is 10 min; entries leftover after that are a leaked
//!      authorize URL or a tab close).
//!
//! Implementation lands in the next commit alongside the admin
//! endpoints — this stub keeps the module path stable.
