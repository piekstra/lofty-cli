//! `lofty api` — raw passthrough. Defaults to the SDK surface with Bearer
//! auth; `--internal` targets the website's platform API (no key attached —
//! only its open endpoints will answer).

use pk_cli_core::CliError;
use pk_cli_http::ApiArgs;

use super::{emit, Ctx};
use crate::client::LoftyClient;
use crate::config::INTERNAL_BASE_URL;

#[derive(clap::Args, Debug)]
pub struct Args {
    #[command(flatten)]
    pub api: ApiArgs,

    /// Target the internal platform API (api.lofty.ai/prod) instead of the
    /// SDK surface. Unauthenticated: only publicly open endpoints answer.
    #[arg(long)]
    pub internal: bool,

    /// Skip the confirmation prompt for non-GET methods.
    #[arg(long)]
    pub force: bool,
}

pub fn run(ctx: &Ctx, args: &Args) -> Result<(), CliError> {
    let method = args.api.parsed_method()?;
    let body = args.api.parsed_body()?;
    // Confirm mutations before touching the keychain (SPEC: exit 6 first).
    let base = if args.internal {
        INTERNAL_BASE_URL.to_string()
    } else {
        ctx.cfg.base_url()
    };
    let url = args.api.url(&base);
    if method != reqwest::Method::GET {
        super::confirm(ctx, args.force, &format!("{method} {url}"))?;
    }
    let client = if args.internal {
        LoftyClient::anonymous(INTERNAL_BASE_URL)?
    } else {
        ctx.client()?
    };
    let payload = client.request(method, &url, body)?;
    emit(ctx, "api-response", payload, pk_cli_core::output::render);
    Ok(())
}
