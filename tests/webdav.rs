#[allow(dead_code)]
#[path = "reload/support.rs"]
mod reload_support;
#[allow(dead_code)]
#[path = "static_files/support.rs"]
mod static_support;
#[allow(dead_code)]
#[path = "http_proxy/support.rs"]
mod support;

mod webdav {
    mod cancellation;
    mod capacity;
    mod clients;
    mod commit;
    mod configuration;
    mod constraints;
    mod disk;
    mod filters;
    mod methods;
    mod permissions;
    mod reload;
    mod retirement;
    mod startup;
    mod support;
    mod traversal;
}
