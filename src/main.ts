import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import QRCode from "qrcode";

// ── Types ─────────────────────────────────────────────────────────────────────

interface AgentStatus {
  paired: boolean;
  relay_url: string;
  hostname: string;
  agent_machine_id: string;
}

interface RelayDefaults {
  relay_url: string;
}

interface PairResult {
  paired: boolean;
  relay_url: string;
  hostname: string;
  agent_machine_id: string;
  link_token: string;
  link_code: string;
}

interface PhoneLinkResult {
  relay_url: string;
  link_token: string;
  link_code: string;
}

interface DesktopPairStartResult {
  relay_url: string;
  desktop_token: string;
  link_token: string;
  link_code: string;
  expires_at: string;
}

// ── View helpers ──────────────────────────────────────────────────────────────

function showPair() {
  document.getElementById("view-pair")!.classList.remove("hidden");
  document.getElementById("view-connected")!.classList.add("hidden");
}

function showConnected(status: AgentStatus | PairResult) {
  document.getElementById("view-pair")!.classList.add("hidden");
  document.getElementById("view-connected")!.classList.remove("hidden");
  document.getElementById("connected-hostname")!.textContent = status.hostname;
  document.getElementById("connected-id")!.textContent = status.agent_machine_id;
}

function setConnState(state: string) {
  const dot = document.getElementById("conn-dot")!;
  const label = document.getElementById("conn-label")!;
  if (state === "connected") {
    dot.className = "dot online";
    label.textContent = "Connected";
  } else if (state === "connecting") {
    dot.className = "dot connecting";
    label.textContent = "Connecting…";
  } else {
    dot.className = "dot offline";
    label.textContent = "Disconnected — retrying…";
  }
}

function setError(message: string | null) {
  const el = document.getElementById("pair-error")!;
  if (message) {
    el.textContent = message;
    el.classList.remove("hidden");
  } else {
    el.textContent = "";
    el.classList.add("hidden");
  }
}

function setQrLoading(isLoading: boolean, statusText?: string) {
  document.getElementById("qr-loading")!.classList.toggle("hidden", !isLoading);
  document.getElementById("qr-canvas")!.classList.toggle("hidden", isLoading);
  if (statusText) {
    document.getElementById("qr-status")!.textContent = statusText;
  }
}

function setManualCode(code: string | null) {
  document.getElementById("manual-code-value")!.textContent = code ?? "----";
}

function setSetupQrLoading(isLoading: boolean, statusText?: string) {
  document.getElementById("setup-qr-loading")!.classList.toggle("hidden", !isLoading);
  document.getElementById("setup-qr-canvas")!.classList.toggle("hidden", isLoading);
  if (statusText) {
    document.getElementById("setup-qr-status")!.textContent = statusText;
  }
}

function setSetupManualCode(code: string | null) {
  document.getElementById("setup-manual-code-value")!.textContent = code ?? "----";
}

// ── QR rendering (connected view) ─────────────────────────────────────────────

async function renderPhoneLinkQr(link?: PhoneLinkResult) {
  setQrLoading(true, "Preparing QR…");
  try {
    const result = link ?? (await invoke<PhoneLinkResult>("get_phone_link"));
    const payload = JSON.stringify({
      relayUrl: result.relay_url,
      linkToken: result.link_token,
      kind: "outpost-phone-link",
    });
    const canvas = document.getElementById("qr-canvas") as HTMLCanvasElement;
    await QRCode.toCanvas(canvas, payload, {
      width: 200,
      margin: 1,
      color: { dark: "#18212B", light: "#FFFDF9" },
    });
    setManualCode(result.link_code || null);
    setQrLoading(false, "Scan with your iPhone to connect.");
  } catch (e: any) {
    setManualCode(null);
    setQrLoading(true, "QR unavailable — check your connection to the relay.");
    console.error("[qr]", e);
  }
}

// ── Pairing (first-time setup) ────────────────────────────────────────────────

let desktopPairToken: string | null = null;
let desktopPairPollTimer: ReturnType<typeof setInterval> | null = null;

function stopDesktopPairPolling() {
  if (desktopPairPollTimer) {
    clearInterval(desktopPairPollTimer);
    desktopPairPollTimer = null;
  }
}

async function renderSetupPairQr(result: DesktopPairStartResult) {
  const payload = JSON.stringify({
    relayUrl: result.relay_url,
    linkToken: result.link_token,
    kind: "outpost-desktop-link",
  });
  const canvas = document.getElementById("setup-qr-canvas") as HTMLCanvasElement;
  await QRCode.toCanvas(canvas, payload, {
    width: 200,
    margin: 1,
    color: { dark: "#18212B", light: "#FFFDF9" },
  });
  setSetupManualCode(result.link_code || null);
  setSetupQrLoading(false, "Scan with your iPhone or enter the code in Outpost.");
}

async function startDesktopPairing() {
  stopDesktopPairPolling();
  desktopPairToken = null;
  setError(null);
  setSetupManualCode(null);
  setSetupQrLoading(true, "Preparing secure pairing...");
  try {
    const result = await invoke<DesktopPairStartResult>("start_desktop_pairing");
    desktopPairToken = result.desktop_token;
    await renderSetupPairQr(result);
    desktopPairPollTimer = setInterval(() => {
      void checkDesktopPairing();
    }, 1800);
  } catch (e: any) {
    setSetupQrLoading(true, "Could not create pairing code. Check the relay URL and refresh.");
    setError(String(e?.message ?? e));
  }
}

async function checkDesktopPairing() {
  if (!desktopPairToken) return;
  try {
    const result = await invoke<PairResult | null>("check_desktop_pairing", {
      desktopToken: desktopPairToken,
    });
    if (!result) return;
    stopDesktopPairPolling();
    desktopPairToken = null;
    showConnected(result);
    setConnState("connecting");
    if (result.link_token) {
      await renderPhoneLinkQr({
        relay_url: result.relay_url,
        link_token: result.link_token,
        link_code: result.link_code,
      });
    } else {
      await renderPhoneLinkQr();
    }
  } catch (e: any) {
    stopDesktopPairPolling();
    desktopPairToken = null;
    setSetupQrLoading(true, "Pairing code expired. Refresh and try again.");
    setError(String(e?.message ?? e));
  }
}

async function handleConnect() {
  const input = document.getElementById("pair-code-input") as HTMLInputElement;
  const code = input.value.trim();
  if (!code) {
    setError("Enter the code shown on your iPhone.");
    return;
  }
  setError(null);
  const btn = document.getElementById("btn-connect") as HTMLButtonElement;
  btn.disabled = true;
  btn.textContent = "Connecting…";

  try {
    const result = await invoke<PairResult>("pair_with_code", { code });
    stopDesktopPairPolling();
    desktopPairToken = null;
    showConnected(result);
    setConnState("connecting");
    // Render the QR immediately using the link token returned by pair_with_code.
    if (result.link_token) {
      await renderPhoneLinkQr({
        relay_url: result.relay_url,
        link_token: result.link_token,
        link_code: result.link_code,
      });
    } else {
      await renderPhoneLinkQr();
    }
  } catch (e: any) {
    setError(String(e?.message ?? e));
  } finally {
    btn.disabled = false;
    btn.textContent = "Connect";
  }
}

// ── Approval UI ───────────────────────────────────────────────────────────────

interface PermissionRequest {
  requestId: string;
  actionType: string;
  description: string;
}

let currentApprovalRequestId: string | null = null;
let approvalCountdownTimer: ReturnType<typeof setInterval> | null = null;

function showApproval(req: PermissionRequest) {
  currentApprovalRequestId = req.requestId;
  document.getElementById("approval-panel")!.classList.remove("hidden");
  document.getElementById("approval-description")!.textContent = req.description;
  document.getElementById("approval-countdown")!.textContent = "30";

  if (approvalCountdownTimer) clearInterval(approvalCountdownTimer);
  let seconds = 30;
  approvalCountdownTimer = setInterval(() => {
    seconds -= 1;
    const el = document.getElementById("approval-countdown");
    if (el) el.textContent = String(seconds);
    if (seconds <= 0) clearApproval();
  }, 1000);
}

function clearApproval() {
  currentApprovalRequestId = null;
  document.getElementById("approval-panel")!.classList.add("hidden");
  document.getElementById("approval-description")!.textContent = "";
  if (approvalCountdownTimer) { clearInterval(approvalCountdownTimer); approvalCountdownTimer = null; }
}

// ── Initialisation ────────────────────────────────────────────────────────────

async function init() {
  const status: AgentStatus = await invoke("get_status");

  // Pre-fill relay URL input with the baked-in or saved default.
  try {
    const defaults = await invoke<RelayDefaults>("get_relay_defaults");
    const relayInput = document.getElementById("relay-url-input") as HTMLInputElement;
    if (relayInput && !relayInput.value) relayInput.value = defaults.relay_url;
  } catch { /* ignore */ }

  if (status.paired) {
    stopDesktopPairPolling();
    showConnected(status);
    setConnState("connecting");
    void renderPhoneLinkQr();
  } else {
    showPair();
    void startDesktopPairing();
  }

  // ── Event listeners ──────────────────────────────────────────────────────

  await listen<string>("connection_state", (e) => setConnState(e.payload));

  await listen<void>("force_unpair", async () => {
    await invoke("unpair");
    showPair();
    void startDesktopPairing();
  });

  document.getElementById("btn-connect")?.addEventListener("click", () => void handleConnect());

  document.getElementById("btn-refresh-setup-qr")?.addEventListener("click", () => {
    void startDesktopPairing();
  });

  document.getElementById("btn-show-phone-code")?.addEventListener("click", () => {
    const panel = document.getElementById("phone-code-panel")!;
    const visible = !panel.classList.contains("hidden");
    panel.classList.toggle("hidden", visible);
    (document.getElementById("btn-show-phone-code") as HTMLButtonElement).textContent = visible
      ? "Enter code from iPhone instead"
      : "Hide phone code entry";
  });

  document.getElementById("pair-code-input")?.addEventListener("keydown", (e) => {
    if (e.key === "Enter") void handleConnect();
  });

  document.getElementById("btn-refresh-qr")?.addEventListener("click", () => {
    void renderPhoneLinkQr();
  });

  document.getElementById("btn-toggle-relay")?.addEventListener("click", () => {
    const panel = document.getElementById("relay-panel")!;
    const visible = !panel.classList.contains("hidden");
    panel.classList.toggle("hidden", visible);
    (document.getElementById("btn-toggle-relay") as HTMLButtonElement).textContent = visible
      ? "Change relay URL"
      : "Hide relay URL";
  });

  document.getElementById("btn-save-relay")?.addEventListener("click", async () => {
    const input = document.getElementById("relay-url-input") as HTMLInputElement;
    const url = input.value.trim();
    if (!url) return;
    try {
      await invoke("update_relay_url", { url });
      document.getElementById("relay-panel")!.classList.add("hidden");
      (document.getElementById("btn-toggle-relay") as HTMLButtonElement).textContent = "Change relay URL";
    } catch (e: any) {
      setError(String(e?.message ?? e));
    }
  });

  document.getElementById("btn-unpair")?.addEventListener("click", async () => {
    if (!confirm("Unpair this PC? You'll need to enter a new code to reconnect.")) return;
    await invoke("unpair");
    showPair();
    void startDesktopPairing();
    const input = document.getElementById("pair-code-input") as HTMLInputElement;
    if (input) input.value = "";
  });

  document.getElementById("btn-approve")?.addEventListener("click", async () => {
    const rid = currentApprovalRequestId;
    clearApproval();
    if (rid) await invoke("approve_action_id", { requestId: rid });
  });

  document.getElementById("btn-deny")?.addEventListener("click", async () => {
    const rid = currentApprovalRequestId;
    clearApproval();
    if (rid) await invoke("deny_action_id", { requestId: rid });
  });

  // Show approval panel when the agent requests user sign-off on a dangerous action.
  await listen<PermissionRequest>("permission_request", (e) => showApproval(e.payload));

  // Clear approval panel once the action is resolved (approved, denied, or timed out).
  await listen<{ requestId: string; approved: boolean }>("permission_resolved", () => clearApproval());
}

init();
