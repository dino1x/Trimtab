use clap::{Parser, Subcommand};
use keeper::{api, backtest, data, intents, live, mpc, strategy::Policy};
use std::time::Duration;

#[derive(Parser)]
#[command(name = "trimtab-keeper", about = "Trimtab keeper: backtest, demo API, live loop")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Serve the API in demo mode (no keys needed).
    Serve {
        #[arg(long, env = "PORT", default_value = "8080")]
        port: u16,
        /// Fetch live venue funding and 1Click dry quotes.
        #[arg(long)]
        online: bool,
    },
    /// MCP server on stdio for AI agents (quote_deposit, deposit_intent, position, receipt).
    Mcp {
        #[arg(long)]
        online: bool,
    },
    /// Run the backtest on the committed data and print the headline table.
    Backtest {
        #[arg(long)]
        json: bool,
    },
    /// Daily receipt: vault vs US T-bills (backtest until live).
    Receipt {
        /// YYYY-MM-DD; default is the last day of the window.
        #[arg(long)]
        date: Option<String>,
    },
    /// Re-download the hourly funding and spot data into ./data.
    FetchHistory,
    /// Ask 1Click for a dry quote (no deposit address, nothing moves).
    Quote {
        #[arg(long, default_value = "25")]
        usdc: f64,
    },
    /// Print the EVM address Chain Signatures derives for a vault account and path.
    Derive {
        #[arg(long)]
        vault: String,
        #[arg(long, default_value = "trimtab-perp-1")]
        path: String,
        /// The MPC contract's `public_key` view (secp256k1:...).
        #[arg(long)]
        root_key: String,
    },
    /// Keyless check of the NEAR testnet run: every logged transaction and the vault's live state.
    Verify {
        #[arg(long, default_value = keeper::verify::DEFAULT_RPC)]
        rpc: String,
    },
    /// One live keeper cycle (testnet, or mainnet with the explicit opt-in).
    Tick,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt().with_writer(std::io::stderr).with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).init();
    match Cli::parse().cmd {
        Cmd::Serve { port, online } => {
            let state = api::AppState::new(online)?;
            api::spawn_ticker(state.clone(), Duration::from_secs(3));
            let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
            println!("keeper API (DEMO) on http://localhost:{port}/api/state");
            axum::serve(listener, api::router(state)).await?;
        }
        Cmd::Mcp { online } => {
            keeper::mcp::serve_stdio(api::AppState::new(online)?).await?;
        }
        Cmd::Backtest { json } => {
            let r = backtest::default_report()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&r)?);
            } else {
                print_report(&r);
            }
        }
        Cmd::Receipt { date } => {
            let r = backtest::default_report()?;
            let x = match date { Some(d) => keeper::receipt::on(&r, &d)?, None => keeper::receipt::latest(&r)? };
            let y = keeper::receipt::year(&r)?;
            println!("{}", serde_json::to_string_pretty(&serde_json::json!({"day": x, "year": y}))?);
        }
        Cmd::FetchHistory => {
            let (f, s) = data::fetch_history(std::path::Path::new("data"), "ETH", "ETHUSDT").await?;
            println!("wrote {f} funding rows and {s} spot rows to data/");
        }
        Cmd::Quote { usdc } => {
            let q = intents::quote(
                &reqwest::Client::new(),
                true,
                intents::USDC_NEAR,
                intents::ETH_NEAR,
                (usdc * 1e6) as u128,
                "intents.near",
                "intents.near",
            )
            .await?;
            println!("{}", serde_json::to_string_pretty(&q)?);
        }
        Cmd::Derive { vault, path, root_key } => {
            let root = mpc::parse_near_secp_key(&root_key)?;
            let pk = mpc::derive_public_key(&root, &vault, &path)?;
            println!("0x{}", hex::encode(mpc::eth_address(&pk)));
        }
        Cmd::Verify { rpc } => {
            let (pass, total) = keeper::verify::run(&rpc).await?;
            if pass != total {
                std::process::exit(1);
            }
        }
        Cmd::Tick => {
            let cfg = live::LiveConfig::from_env()?;
            live::tick(&cfg, &Policy::default()).await?;
        }
    }
    Ok(())
}

fn print_report(r: &backtest::Report) {
    println!("{}", r.label);
    println!("window            {} hours", r.hours);
    println!("start → end NAV   {:.2} → {:.2} USDC", r.start_nav, r.end_nav);
    println!("net APR           {:.2}%", r.net_apr_pct);
    println!("APR last 30d      {:.2}%", r.apr_30d_pct);
    println!("APR last 7d       {:.2}%", r.apr_7d_pct);
    println!("funding income    {:.2}", r.funding_income);
    println!("costs paid        {:.2}", r.costs_paid);
    println!("basis PnL         {:.2}", r.basis_pnl);
    println!("max drawdown      {:.2}%", r.max_drawdown_pct);
    println!("max hedge drift   {:.3}%", r.max_drift_pct);
    println!("final net delta   {:.3}%", r.final_net_delta_pct);
    println!("max leverage      {:.2}x", r.max_leverage);
    println!("min margin ratio  {:.2}%", r.min_margin_ratio_pct);
    println!("rebalances        {}", r.rebalances);
    println!("neg-funding hours {:.1}%", r.negative_funding_hours_pct);
    println!("mean funding APR  {:.2}%", r.mean_funding_apr_pct);
    for m in &r.months {
        println!("  {}  funding {:>6.2}%  vault {:>6.2}%", m.month, m.funding_apr_pct, m.vault_return_pct);
    }
}
