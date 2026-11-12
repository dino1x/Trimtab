# Trimtab: 2-minute demo script

Narrated, with on-screen captions so it works muted. Record at 1440×900 against a local run (`cargo leptos watch`) plus one terminal. Every number on screen comes from the app or the tests; say "backtest" whenever a backtest number is shown.

| Time | Screen | Narration | Caption |
|---|---|---|---|
| 0:00–0:10 | Hero: headline and the glowing hedge gauge | "Trimtab is a dollar vault that earns what perp traders pay to stay long, while the price of ETH cancels out." | Yield that doesn't care where price goes |
| 0:10–0:25 | Slowly zoom on the gauge readout, then the pill nav (APY, Δ, DEMO) | "It holds spot ETH and an equal short. The gauge is net delta against a ±100 bps band. This page runs a demo vault, labelled DEMO, replaying real hours." | Net delta +0.000% · band ±100 bps · DEMO |
| 0:25–0:40 | Funding bars section | "Here is where the yield comes from: a year of real hourly funding. Mostly positive; in April shorts paid. Every bar is real data, with simulated execution." | Real hourly funding, Nov 2025 – Oct 2026 |
| 0:40–0:55 | Daily receipt card | "The daily receipt compares the vault with US Treasury bills on the same $10,000. Over the backtest year it earned $367 against $385 for T-bills: ahead on 197 days, behind over the year. We show that, not a bigger number." | Backtest: $367.17 vs T-bills $385.41 |
| 0:55–1:10 | Deposit card: type `alice.near`, `250`, press Deposit | "Deposits mint shares at NAV per share, with the contract's own math. On mainnet this is one ft_transfer_call of USDC." | Shares mint at NAV/share |
| 1:10–1:20 | Click Connect wallet; the selector opens | "Any NEAR wallet, through Wallet Selector." | NEAR Wallet Selector 10 |
| 1:20–1:45 | Terminal: `cargo nextest run -p vault-contract --test sandbox break_it`, then open `docs/break-it.md` | "Now we attack it. A stolen keeper key tries to over-short, trade off market, replay a nonce, inflate NAV. The owner tries to redirect margin. Nineteen attacks on a real NEAR sandbox, nineteen refused." | 19 of 19 refused |
| 1:45–1:55 | Guardrails card, then the "Remove NEAR and this stops" table in the README | "Intents builds both legs from the contract. Chain Signatures lets the contract own the perp key, so there's no hot wallet. And the contract holds every limit; the keeper can only ask." | Intents · Chain Signatures · the contract |
| 1:55–2:00 | "Where it stands" table | "Not on mainnet yet, and the README says so, line by line." | github repo · MIT |

## Shot checklist
- Start the app fresh so the demo replay begins at hour 0.
- Terminal font ≥ 18 px; hide the shell prompt's path.
- Don't show `.env` or any key; there are none in the demo.
- If the network is slow, record the sandbox run beforehand; it takes about 75 s.
