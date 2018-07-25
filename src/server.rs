//! Main application server
use std::sync::{Arc, RwLock};
use std::collections::HashMap;

use actix::{SyncArbiter, System, SystemRunner};
use actix_web::{http, middleware::cors::Cors, server::HttpServer, App};
use num_cpus;

use db::models::DBManager;
use dispatcher::DBExecutor;
use handlers;
use handlers::ServerState;
use settings::Settings;

pub struct Server {}

impl Server {
    pub fn with_settings(settings: &Settings) -> SystemRunner {
        let sys = System::new("syncserver");

        // Start dispatcher with the arbiter
        let db_pool = Arc::new(RwLock::new(HashMap::new()));
        let db_executor = SyncArbiter::start(num_cpus::get(), move || {
            DBExecutor { db_handles: db_pool.clone() }
        });

        HttpServer::new(move || {
            // Setup the server state
            let state = ServerState {
                db_executor: db_executor.clone(),
            };

            App::with_state(state)
                // HTTP handler routes
                .configure(|app| {
                    Cors::for_app(app)
                        .resource(
                            "{uid}/info/collections", |r| {
                                r.method(http::Method::GET)
                                    .with(handlers::collection_info)
                            })
                        .register()
                })
        }).bind(format!("127.0.0.1:{}", settings.port))
            .unwrap()
            .start();
        sys
    }
}
