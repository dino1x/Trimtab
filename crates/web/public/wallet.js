// Wallet shim: NEAR Wallet Selector 10 loaded as ES modules from a CDN.
// The only JavaScript in the repo. It connects a wallet, fills the account
// field, and (when TRIMTAB_VAULT is configured) sends the real deposit:
// ft_transfer_call(USDC -> vault, msg "deposit").
const V = "10.1.4";
const cdn = (p) => `https://esm.sh/@near-wallet-selector/${p}@${V}`;
const cfg = { network: "mainnet", vault: document.documentElement.dataset.vault || "" };
let selector, modal;

async function init() {
  const [{ setupWalletSelector }, { setupModal }, { setupMeteorWallet }, { setupMyNearWallet }, { setupHotWallet }, { setupIntearWallet }] =
    await Promise.all([cdn("core"), cdn("modal-ui"), cdn("meteor-wallet"), cdn("my-near-wallet"), cdn("hot-wallet"), cdn("intear-wallet")].map((u) => import(u)));
  const css = document.createElement("link");
  css.rel = "stylesheet"; css.href = `https://cdn.jsdelivr.net/npm/@near-wallet-selector/modal-ui@${V}/styles.css`;
  document.head.appendChild(css);
  selector = await setupWalletSelector({ network: cfg.network, modules: [setupMeteorWallet(), setupMyNearWallet(), setupHotWallet(), setupIntearWallet()] });
  modal = setupModal(selector, { contractId: cfg.vault || "intents.near" });
  selector.store.observable.subscribe((s) => render(s.accounts?.[0]?.accountId));
  render(selector.store.getState().accounts?.[0]?.accountId);
}

function render(accountId) {
  document.querySelectorAll('[data-wallet="connect"]').forEach((b) => { b.textContent = accountId ? accountId : "Connect wallet"; });
  if (!accountId) return;
  document.querySelectorAll('[data-wallet="account"]').forEach((i) => {
    if (!i.value) { i.value = accountId; i.dispatchEvent(new Event("input", { bubbles: true })); }
  });
}

document.addEventListener("click", async (e) => {
  const b = e.target.closest('[data-wallet="connect"]');
  if (!b) return;
  try {
    if (!selector) { b.textContent = "Loading…"; await init(); }
    if (selector.isSignedIn()) { const w = await selector.wallet(); await w.signOut(); render(undefined); }
    else modal.show();
  } catch (err) {
    b.textContent = "Wallet unavailable";
    console.error(err);
  }
});

window.trimtabWallet = { init, get selector() { return selector; } };
