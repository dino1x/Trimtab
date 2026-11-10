use crate::model::*;
use leptos::prelude::*;
use leptos_meta::{provide_meta_context, Link, Meta, MetaTags, Stylesheet, Title};
use leptos_router::components::{Route, Router, Routes};
use leptos_router::StaticSegment;

pub fn shell(options: LeptosOptions) -> impl IntoView {
    view! {
        <!DOCTYPE html>
        <html lang="en">
            <head>
                <meta charset="utf-8"/>
                <meta name="viewport" content="width=device-width, initial-scale=1"/>
                <AutoReload options=options.clone()/>
                <HydrationScripts options/>
                <MetaTags/>
            </head>
            <body>
                <App/>
            </body>
        </html>
    }
}

#[server]
pub async fn get_dash() -> Result<Dash, ServerFnError> {
    let state = use_context::<keeper::api::AppState>().ok_or_else(|| ServerFnError::new("keeper unavailable"))?;
    let snap: Snap = serde_json::from_value(serde_json::to_value(state.snapshot())?)?;
    let bt: Bt = serde_json::from_value(serde_json::to_value(&*state.report)?)?;
    Ok(Dash { snap, bt })
}

#[server]
pub async fn demo_deposit(account: String, usd: f64) -> Result<String, ServerFnError> {
    let state = use_context::<keeper::api::AppState>().ok_or_else(|| ServerFnError::new("keeper unavailable"))?;
    let account = account.trim().to_lowercase();
    if account.is_empty() || account.len() > 64 || !account.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)) {
        return Err(ServerFnError::new("Enter a NEAR account like alice.near"));
    }
    let mut d = state.demo.lock().unwrap();
    let minted = d.deposit(&account, usd).map_err(|e| ServerFnError::new(e.to_string()))?;
    Ok(format!("Minted {:.4} shares to {account} (demo vault, no funds moved)", minted as f64 / 1e6))
}

#[component]
pub fn App() -> impl IntoView {
    provide_meta_context();
    view! {
        <Stylesheet id="leptos" href="/pkg/trimtab.css"/>
        <Link rel="preconnect" href="https://fonts.googleapis.com"/>
        <Link rel="stylesheet" href="https://fonts.googleapis.com/css2?family=Geist:wght@400&family=Geist+Mono:wght@400&display=swap"/>
        <Link rel="icon" href="/favicon.svg"/>
        <Title text="Trimtab: delta-neutral vault on NEAR"/>
        <Meta name="description" content="Deposit USDC; the vault holds spot ETH and an equal short perp, and earns funding. Guardrails enforced by a NEAR contract."/>
        <script type="module" src="/wallet.js"></script>
        <Router>
            <Routes fallback=|| view! { <p class="wrap">"Page not found."</p> }>
                <Route path=StaticSegment("") view=Home/>
            </Routes>
        </Router>
    }
}

#[component]
fn Home() -> impl IntoView {
    let dash = Resource::new(|| (), |_| get_dash());
    Effect::new(move |_| {
        let _ = set_interval_with_handle(move || dash.refetch(), std::time::Duration::from_secs(4));
    });
    let view_dash = move |f: fn(Dash) -> AnyView| {
        move || {
            Suspend::new(async move {
                match dash.await {
                    Ok(d) => f(d),
                    Err(_) => view! {
                        <div class="err" role="alert">
                            <b>"Keeper unreachable."</b>" The page retries every 4 seconds. Numbers below the fold are paused, not zero."
                        </div>
                    }
                    .into_any(),
                }
            })
        }
    };

    view! {
        <Transition fallback=|| view! { <Nav loading=true snap=Snap::default()/> }>
            {view_dash(|d| view! { <Nav loading=false snap=d.snap/> }.into_any())}
        </Transition>
        <main>
            <section class="hero">
                <div class="glow" aria-hidden="true"></div>
                <div class="wrap">
                    <p class="eyebrow">"Delta-neutral vault · NEAR"</p>
                    <h1>"Yield that doesn't care" <br/> "where price goes."</h1>
                    <p class="lede">
                        "Deposit USDC. The vault buys spot ETH through NEAR Intents and opens an equal short perp through Chain Signatures. "
                        "Price moves cancel out; funding paid to shorts is the yield. A NEAR contract holds the guardrails."
                    </p>
                    <Transition fallback=|| view! { <GaugeSkeleton/> }>
                        {view_dash(|d| view! { <Hero d/> }.into_any())}
                    </Transition>
                </div>
            </section>

            <section class="split" id="funding">
                <div class="wrap two">
                    <div>
                        <p class="eyebrow">"Where the yield comes from"</p>
                        <h2>"Funding, month by month."</h2>
                        <p class="muted">
                            "Perp longs pay shorts when the market leans long. Bars show each month's average funding rate, annualized, "
                            "and what the simulated vault actually returned after fees. Real hourly data; simulated execution."
                        </p>
                    </div>
                    <Transition fallback=|| view! { <div class="bars sk" style="height:320px"></div> }>
                        {view_dash(|d| view! { <Bars bt=d.bt/> }.into_any())}
                    </Transition>
                </div>
            </section>

            <section id="vault">
                <div class="wrap">
                    <p class="eyebrow">"The vault"</p>
                    <h2>"Three panels, one position."</h2>
                    <Transition fallback=|| view! { <div class="cards"><div class="card sk"></div><div class="card sk"></div><div class="card sk"></div></div> }>
                        {view_dash(|d| view! { <Cards snap=d.snap/> }.into_any())}
                    </Transition>
                </div>
            </section>

            <section id="proof">
                <div class="wrap">
                    <p class="eyebrow">"Backtest, labelled as such"</p>
                    <h2>"Twelve months, replayed hour by hour."</h2>
                    <Transition fallback=|| view! { <div class="big3 sk" style="height:140px"></div> }>
                        {view_dash(|d| view! { <Proof bt=d.bt events=d.snap.events/> }.into_any())}
                    </Transition>
                </div>
            </section>

            <section id="dev">
                <div class="wrap two">
                    <div>
                        <p class="eyebrow">"For developers"</p>
                        <h2>"Everything is an endpoint."</h2>
                        <p class="muted">"The keeper serves the same numbers this page shows. No key needed in demo mode."</p>
                        <ul class="eps mono">
                            <li><span>"GET"</span>" /api/state"</li>
                            <li><span>"GET"</span>" /api/backtest"</li>
                            <li><span>"GET"</span>" /api/market"</li>
                            <li><span>"GET"</span>" /api/quote"</li>
                            <li><span>"POST"</span>" /api/demo/deposit"</li>
                        </ul>
                    </div>
                    <Transition fallback=|| view! { <pre class="code sk"></pre> }>
                        {view_dash(|d| {
                            let s = &d.snap;
                            let body = format!(
                                "{{\n  \"mode\": \"{}\",\n  \"nav_usd\": {:.2},\n  \"spot_qty\": {:.4},\n  \"short_qty\": {:.4},\n  \"drift_bps\": {:.2},\n  \"hedge_band_bps\": {:.0},\n  \"leverage\": {:.2},\n  \"funding_apr_pct\": {:.2}\n}}",
                                s.mode, s.nav_usd, s.spot_qty, s.short_qty, s.drift_bps, s.hedge_band_bps, s.leverage, s.funding_apr_pct
                            );
                            view! { <pre class="code mono" aria-label="GET /api/state response">{body}</pre> }.into_any()
                        })}
                    </Transition>
                </div>
            </section>
        </main>
        <footer>
            <div class="wrap foot">
                <span>"Trimtab · MIT · "</span>
                <span class="muted">"Demo mode: the vault on this page is simulated and no funds move. Backtest numbers are simulated execution on real data, not live returns."</span>
            </div>
        </footer>
    }
}

#[component]
fn Nav(loading: bool, snap: Snap) -> impl IntoView {
    let apy = if loading { "—".to_string() } else { format!("{:.1}%", snap.apy_7d_pct) };
    let delta = if loading { "—".to_string() } else { fmt_signed_pct(snap.net_delta_pct, 2) };
    view! {
        <nav class="pill" aria-label="Main">
            <a class="brand" href="/">
                <svg width="18" height="18" viewBox="0 0 24 24" aria-hidden="true"><path d="M3 15a9 9 0 0 1 18 0" fill="none" stroke="currentColor" stroke-width="1.6"/><path d="M12 15l4-6" stroke="currentColor" stroke-width="1.6"/></svg>
                "Trimtab"
            </a>
            <div class="chips mono">
                <i title="Backtest APR over the last 7 replayed days">"APY 7d "<b>{apy}</b></i>
                <i title="Net delta as a share of NAV">"Δ "<b>{delta}</b></i>
                {(!loading).then(|| view! { <i class="demo">"DEMO"</i> })}
            </div>
            <button class="btn" data-wallet="connect" type="button">"Connect wallet"</button>
        </nav>
    }
}

#[component]
fn GaugeSkeleton() -> impl IntoView {
    view! { <div class="gauge sk" aria-busy="true" aria-label="Loading hedge gauge"></div> }
}

#[component]
fn Hero(d: Dash) -> impl IntoView {
    let s = d.snap;
    let bt = d.bt;
    let span = s.hedge_band_bps * 2.0; // gauge covers ±2 bands
    let signed_bps = if s.net_delta_usd < 0.0 { -s.drift_bps } else { s.drift_bps };
    let x = (signed_bps / span).clamp(-1.0, 1.0);
    let ang = std::f64::consts::PI * (1.0 - (x + 1.0) / 2.0);
    let (cx, cy, r) = (300.0, 300.0, 240.0);
    let nx = cx + r * ang.cos();
    let ny = cy - r * ang.sin();
    let band_a = std::f64::consts::PI * (1.0 - (0.5 + 1.0) / 2.0);
    let band_b = std::f64::consts::PI * (1.0 - (-0.5 + 1.0) / 2.0);
    let arc = |a0: f64, a1: f64, rr: f64| {
        format!(
            "M {:.1} {:.1} A {rr} {rr} 0 0 1 {:.1} {:.1}",
            cx + rr * a1.cos(), cy - rr * a1.sin(), cx + rr * a0.cos(), cy - rr * a0.sin()
        )
    };
    let inside = s.drift_bps <= s.hedge_band_bps;
    view! {
        <figure class="gauge" aria-label=format!("Hedge drift {:.2} basis points, band {:.0}", s.drift_bps, s.hedge_band_bps)>
            <svg viewBox="0 0 600 330" role="img">
                <defs>
                    <filter id="g" x="-50%" y="-50%" width="200%" height="200%"><feGaussianBlur stdDeviation="9"/></filter>
                </defs>
                <path d=arc(0.0, std::f64::consts::PI, r) class="track"/>
                <path d=arc(band_a, band_b, r) class="band-glow" filter="url(#g)"/>
                <path d=arc(band_a, band_b, r) class="band"/>
                <line x1=cx y1=cy x2=format!("{nx:.1}") y2=format!("{ny:.1}") class="needle"/>
                <circle cx=format!("{nx:.1}") cy=format!("{ny:.1}") r="7" class="tip"/>
                <text x="60" y="324" class="tick">{format!("−{:.0} bps", span)}</text>
                <text x="540" y="324" class="tick" text-anchor="end">{format!("+{:.0} bps", span)}</text>
            </svg>
            <figcaption>
                <span class="big">{fmt_signed_pct(s.net_delta_pct, 3)}</span>
                <span class="mono small">{format!("net delta · drift {:.2} bps · band ±{:.0} bps · ", s.drift_bps, s.hedge_band_bps)}
                    <b class=if inside { "ok" } else { "warn" }>{if inside { "inside band" } else { "rebalancing" }}</b>
                </span>
            </figcaption>
        </figure>
        <div class="stats">
            <div><b>{format!("{:.2}%", bt.net_apr_pct)}</b><small>"Net APR · 12-month backtest"</small></div>
            <div><b>{format!("{:.2}%", bt.max_drawdown_pct)}</b><small>"Max drawdown · backtest"</small></div>
            <div><b>{format!("{:.3}%", bt.max_drift_pct)}</b><small>"Max hedge drift · backtest"</small></div>
        </div>
        <p class="fine mono">{format!("{} · {} → {}", bt.label, fmt_date(bt.start_ms), fmt_date(bt.end_ms))}</p>
    }
}

#[component]
fn Bars(bt: Bt) -> impl IntoView {
    let max = bt.months.iter().map(|m| m.funding_apr_pct.abs()).fold(1.0f64, f64::max);
    view! {
        <div class="bars" role="table" aria-label="Monthly funding APR and vault return">
            <div class="bar-head mono" role="row"><span>"Month"</span><span>"Funding APR"</span><span>"Vault"</span></div>
            {bt.months.into_iter().map(|m| {
                let w = (m.funding_apr_pct.abs() / max * 100.0).max(1.0);
                let neg = m.funding_apr_pct < 0.0;
                view! {
                    <div class="bar-row" role="row">
                        <span class="mono">{m.month.clone()}</span>
                        <span class="track"><span class=if neg { "fill neg" } else { "fill" } style=format!("width:{w:.1}%")></span>
                            <em class="mono">{format!("{:.1}%", m.funding_apr_pct)}</em></span>
                        <span class="mono">{fmt_signed_pct(m.vault_return_pct, 2)}</span>
                    </div>
                }
            }).collect_view()}
        </div>
    }
}

#[component]
fn Cards(snap: Snap) -> impl IntoView {
    let s = snap;
    let (account, set_account) = signal(String::from(""));
    let (amount, set_amount) = signal(String::from("100"));
    let deposit = ServerAction::<DemoDeposit>::new();
    let pending = deposit.pending();
    let result = deposit.value();
    let pps = s.price_per_share;
    let preview = move || {
        amount.get().parse::<f64>().ok().filter(|v| *v > 0.0).map(|v| format!("≈ {:.4} shares at {:.6} USDC/share", v / pps, pps))
            .unwrap_or_else(|| "Enter an amount".into())
    };
    let spot_val = s.spot_qty * s.px;
    let short_val = s.short_qty * s.px;
    let maxv = spot_val.max(short_val).max(1.0);
    let rebal = format!("{}/{}", s.epoch_rebalances, s.max_rebalances_per_epoch);
    view! {
        <div class="cards">
            <article class="card">
                <header><h3>"Deposit"</h3><span class="tag mono">"DEMO VAULT"</span></header>
                <form class="ui" on:submit=move |ev| {
                    ev.prevent_default();
                    let usd = amount.get_untracked().parse::<f64>().unwrap_or(0.0);
                    deposit.dispatch(DemoDeposit { account: account.get_untracked(), usd });
                }>
                    <label for="acct">"NEAR account"</label>
                    <input id="acct" name="account" data-wallet="account" placeholder="alice.near" autocomplete="off"
                        prop:value=account on:input=move |e| set_account.set(event_target_value(&e))/>
                    <label for="amt">"Amount (USDC)"</label>
                    <div class="amt"><input id="amt" inputmode="decimal" prop:value=amount on:input=move |e| set_amount.set(event_target_value(&e))/><span class="mono">"USDC"</span></div>
                    <p class="mono small muted">{preview}</p>
                    <button class="btn wide" type="submit" disabled=pending>{move || if pending.get() { "Minting…" } else { "Deposit to demo vault" }}</button>
                    <p class="mono small" aria-live="polite">{move || match result.get() {
                        None => view! { <span class="muted">"No deposit yet. Shares mint at NAV per share, rounded in the vault's favour."</span> }.into_any(),
                        Some(Ok(m)) => view! { <span class="ok">{m}</span> }.into_any(),
                        Some(Err(e)) => view! { <span class="warn">{e.to_string().replace("error running server function: ", "")}</span> }.into_any(),
                    }}</p>
                </form>
                <p class="cap">"Real vault: one ft_transfer_call of USDC with msg \"deposit\". Here the keeper's demo vault runs the contract's share math."</p>
            </article>

            <article class="card">
                <header><h3>"Position"</h3><span class="tag mono">{format!("ETH {}", fmt_usd(s.px))}</span></header>
                <div class="legs">
                    <div class="leg"><span>"Spot long · NEAR Intents"</span><b class="mono">{format!("{:.4} ETH", s.spot_qty)}</b>
                        <span class="lbar"><span style=format!("width:{:.1}%", spot_val / maxv * 100.0)></span></span></div>
                    <div class="leg"><span>"Perp short · Chain Signatures"</span><b class="mono">{format!("{:.4} ETH", s.short_qty)}</b>
                        <span class="lbar short"><span style=format!("width:{:.1}%", short_val / maxv * 100.0)></span></span></div>
                </div>
                <dl class="kv mono">
                    <dt>"NAV"</dt><dd>{fmt_usd(s.nav_usd)}</dd>
                    <dt>"Margin"</dt><dd>{fmt_usd(s.margin_equity_usd)}</dd>
                    <dt>"Leverage"</dt><dd>{format!("{:.2}x", s.leverage)}</dd>
                    <dt>"Funding now"</dt><dd>{format!("{:.2}% APR", s.funding_apr_pct)}</dd>
                </dl>
                <p class="cap">"Two legs, equal size. The spot balance is read from the token contract, not reported by the keeper."</p>
            </article>

            <article class="card">
                <header><h3>"Guardrails"</h3><span class="tag mono">{s.vault_mode.clone()}</span></header>
                <ul class="rails mono">
                    <li><span>"Hedge band"</span><b class="ok">{format!("±{:.0} bps", s.hedge_band_bps)}</b></li>
                    <li><span>"Max drawdown"</span><b class="ok">"5.00% → unwind"</b></li>
                    <li><span>"Drawdown now"</span><b>{format!("{:.2}%", s.drawdown_pct)}</b></li>
                    <li><span>"Rebalances / epoch"</span><b>{rebal}</b></li>
                    <li><span>"NAV move / report"</span><b class="ok">"≤ 3.00%"</b></li>
                    <li><span>"Order price vs mark"</span><b class="ok">"≤ 1.00%"</b></li>
                    <li><span>"Keepers"</span><b class="ok">"allowlist"</b></li>
                    <li><span>"Withdrawals"</span><b class="ok">"never paused"</b></li>
                </ul>
                <p class="cap">"The contract hashes every perp order itself before Chain Signatures signs it. A keeper can only trade inside these lines."</p>
            </article>
        </div>
    }
}

#[component]
fn Proof(bt: Bt, events: Vec<Ev>) -> impl IntoView {
    view! {
        <div class="big3">
            <div><b>{format!("{:.2}%", bt.mean_funding_apr_pct)}</b><small>"Mean funding APR, ETH perp"</small></div>
            <div><b>{format!("{:.1}%", bt.negative_funding_hours_pct)}</b><small>"Hours when shorts paid"</small></div>
            <div><b>{bt.rebalances.to_string()}</b><small>"Rebalances in 12 months"</small></div>
        </div>
        <div class="two log-wrap">
            <dl class="kv mono">
                <dt>"Funding earned"</dt><dd>{fmt_usd(bt.funding_income)}" on $10,000"</dd>
                <dt>"Fees paid"</dt><dd>{fmt_usd(bt.costs_paid)}</dd>
                <dt>"Max leverage"</dt><dd>{format!("{:.2}x", bt.max_leverage)}</dd>
                <dt>"Min margin ratio"</dt><dd>{format!("{:.1}%", bt.min_margin_ratio_pct)}</dd>
                <dt>"APR, last 30 days"</dt><dd>{format!("{:.2}%", bt.apr_30d_pct)}</dd>
            </dl>
            <div class="log">
                <h3>"Keeper log · demo replay"</h3>
                {if events.is_empty() {
                    view! { <p class="muted small">"Quiet so far: no rebalance or deposit in this replay window yet."</p> }.into_any()
                } else {
                    view! { <ul class="mono small">{events.into_iter().map(|e| view! { <li><span class="muted">{fmt_date(e.t)}</span>" "<b>{e.kind}</b>" "{e.detail}</li> }).collect_view()}</ul> }.into_any()
                }}
            </div>
        </div>
    }
}
