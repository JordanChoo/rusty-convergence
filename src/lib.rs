use worker::*;

pub mod convergence;
pub mod error;
pub mod metrics;
pub mod prompt;
pub mod providers;
pub mod routes;
pub mod storage;
pub mod types;
pub mod validation;

use error::json_error;
use validation::{validate_auth, validate_csvkey};

#[event(fetch)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    console_error_panic_hook::set_once();
    handle_request(req, env).await
}

macro_rules! require_auth {
    ($url:expr, $env:expr) => {{
        let csvkey = match validate_csvkey($url) {
            Ok(k) => k,
            Err(resp) => return Ok(resp),
        };
        let secret = match $env.secret("CSVKEY") {
            Ok(s) => s.to_string(),
            Err(_) => {
                console_log!("missing_secret: CSVKEY");
                return json_error(500, "Server configuration error", "missing_config", None);
            }
        };
        if let Err(resp) = validate_auth(&csvkey, &secret) {
            return Ok(resp);
        }
        console_log!("auth_ok");
        $env.kv("APRP")?
    }};
}

async fn handle_request(req: Request, env: Env) -> Result<Response> {
    let url = req.url()?;
    let method = req.method();
    let path = url.path();
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

    let start_ms = Date::now().as_millis();
    console_log!("request_received method={} path={}", method, path);

    let handler_name = match segments.as_slice() {
        [] | ["health"] => "health",
        ["workflows"] => "workflows",
        ["workflows", _] => "workflows/:name",
        ["documents", _, _] => "documents/:workflow/:role",
        ["run", _, _] => "run/:workflow/:round",
        ["rounds", _, _] => "rounds/:workflow/:round",
        ["rounds", _] => "rounds/:workflow",
        ["stats", _, "rebuild"] => "stats/:workflow/rebuild",
        ["stats", _] => "stats/:workflow",
        ["integrate", _, _] => "integrate/:workflow/:round",
        ["auto", _] => "auto/:workflow",
        _ => "not_found",
    };
    console_log!("route_dispatch handler={}", handler_name);

    let result = match segments.as_slice() {
        [] | ["health"] => match method {
            Method::Get => routes::health::handle(env, &url).await,
            _ => json_error(405, "Method not allowed", "method_not_allowed", None),
        },

        ["workflows"] => {
            let kv = require_auth!(&url, &env);
            match method {
                Method::Get => routes::workflows::handle_list(kv, &url).await,
                Method::Post => routes::workflows::handle_create(kv, req).await,
                _ => json_error(405, "Method not allowed", "method_not_allowed", None),
            }
        }

        ["workflows", name] => {
            let kv = require_auth!(&url, &env);
            match method {
                Method::Get => routes::workflows::handle_get(kv, name).await,
                Method::Delete => routes::workflows::handle_delete(kv, name).await,
                _ => json_error(405, "Method not allowed", "method_not_allowed", None),
            }
        }

        ["documents", workflow, role] => {
            let kv = require_auth!(&url, &env);
            match method {
                Method::Put => {
                    routes::documents::handle_upload(kv, &env, workflow, role, req).await
                }
                Method::Get => routes::documents::handle_get(kv, workflow, role).await,
                _ => json_error(405, "Method not allowed", "method_not_allowed", None),
            }
        }

        ["run", workflow, round_str] => match method {
            Method::Post => {
                let kv = require_auth!(&url, &env);
                routes::run::handle(kv, &env, workflow, round_str, req).await
            }
            _ => json_error(405, "Method not allowed", "method_not_allowed", None),
        },

        ["rounds", workflow, round_str] => match method {
            Method::Get => {
                let kv = require_auth!(&url, &env);
                routes::rounds::handle_get(kv, &env, workflow, round_str, &req).await
            }
            _ => json_error(405, "Method not allowed", "method_not_allowed", None),
        },

        ["rounds", workflow] => match method {
            Method::Get => {
                let kv = require_auth!(&url, &env);
                routes::rounds::handle_list(kv, &env, workflow, &url).await
            }
            _ => json_error(405, "Method not allowed", "method_not_allowed", None),
        },

        ["stats", workflow, "rebuild"] => match method {
            Method::Post => {
                let kv = require_auth!(&url, &env);
                routes::stats::handle_rebuild(kv, workflow).await
            }
            _ => json_error(405, "Method not allowed", "method_not_allowed", None),
        },

        ["stats", workflow] => match method {
            Method::Get => {
                let kv = require_auth!(&url, &env);
                routes::stats::handle_get(kv, workflow).await
            }
            _ => json_error(405, "Method not allowed", "method_not_allowed", None),
        },

        ["integrate", workflow, round_str] => match method {
            Method::Post => {
                let kv = require_auth!(&url, &env);
                routes::integrate::handle(kv, workflow, round_str).await
            }
            _ => json_error(405, "Method not allowed", "method_not_allowed", None),
        },

        ["auto", workflow] => match method {
            Method::Post => {
                let kv = require_auth!(&url, &env);
                routes::auto::handle(kv, &env, workflow, req).await
            }
            _ => json_error(405, "Method not allowed", "method_not_allowed", None),
        },

        _ => json_error(404, "Not found", "not_found", None),
    };

    let elapsed = Date::now().as_millis() - start_ms;
    match &result {
        Ok(resp) => {
            console_log!(
                "request_completed status={} duration_ms={}",
                resp.status_code(),
                elapsed
            );
        }
        Err(e) => {
            console_log!("request_error error={} duration_ms={}", e, elapsed);
        }
    }

    result
}
