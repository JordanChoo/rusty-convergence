use worker::*;

mod error;
mod types;
mod validation;
mod storage;
mod convergence;
mod metrics;
mod prompt;
mod routes;
mod providers;

#[event(fetch)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    console_error_panic_hook::set_once();

    handle_request(req, env).await
}

async fn handle_request(_req: Request, _env: Env) -> Result<Response> {
    Response::ok("rusty-convergence v0.1.0")
}
