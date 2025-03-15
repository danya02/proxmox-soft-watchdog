use std::sync::{Arc, Mutex};

use crate::config;

pub struct Api {
    inner: Arc<Inner>,
    cached_ticket: Option<String>,
}

struct Inner {
    base_url: String,
    username: String,
    password: String,
    ticket: Mutex<Option<String>>,
}

impl Api {
    pub fn from_config(conf: &config::ProxmoxAuth) -> Self {
        Self {
            inner: Arc::new(Inner {
                base_url: conf.url.clone(),
                username: conf.user.clone(),
                password: conf.password.clone(),
                ticket: Mutex::new(None),
            }),
            cached_ticket: None,
        }
    }
}
