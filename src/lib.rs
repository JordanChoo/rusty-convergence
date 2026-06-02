use worker::*;

mod convergence;
mod error;
mod metrics;
mod prompt;
mod providers;
mod routes;
mod storage;
mod types;
mod validation;

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
        $env.kv("APRP")?
    }};
}

async fn handle_request(req: Request, env: Env) -> Result<Response> {
    let url = req.url()?;
    let method = req.method();
    let path = url.path();
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

    match segments.as_slice() {
        [] | ["health"] => {
            if method != Method::Get {
                return json_error(405, "Method not allowed", "method_not_allowed", None);
            }
            routes::health::handle(env).await
        }

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

        ["run", workflow, round_str] => {
            if method != Method::Post {
                return json_error(405, "Method not allowed", "method_not_allowed", None);
            }
            let kv = require_auth!(&url, &env);
            routes::run::handle(kv, &env, workflow, round_str, req).await
        }

        ["rounds", workflow, round_str] => {
            if method != Method::Get {
                return json_error(405, "Method not allowed", "method_not_allowed", None);
            }
            let kv = require_auth!(&url, &env);
            routes::rounds::handle_get(kv, &env, workflow, round_str).await
        }

        ["rounds", workflow] => {
            if method != Method::Get {
                return json_error(405, "Method not allowed", "method_not_allowed", None);
            }
            let kv = require_auth!(&url, &env);
            routes::rounds::handle_list(kv, &env, workflow, &url).await
        }

        ["stats", workflow, "rebuild"] => {
            if method != Method::Post {
                return json_error(405, "Method not allowed", "method_not_allowed", None);
            }
            let kv = require_auth!(&url, &env);
            routes::stats::handle_rebuild(kv, workflow).await
        }

        ["stats", workflow] => {
            if method != Method::Get {
                return json_error(405, "Method not allowed", "method_not_allowed", None);
            }
            let kv = require_auth!(&url, &env);
            routes::stats::handle_get(kv, workflow).await
        }

        ["integrate", workflow, round_str] => {
            if method != Method::Post {
                return json_error(405, "Method not allowed", "method_not_allowed", None);
            }
            let kv = require_auth!(&url, &env);
            routes::integrate::handle(kv, workflow, round_str).await
        }

        _ => json_error(404, "Not found", "not_found", None),
    }
}
