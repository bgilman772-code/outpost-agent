import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import QRCode from "qrcode";

interface AgentStatus {
  paired: boolean;
  relay_url: string;
  hostname: string;
  agent_machine_id: string;
}

interface RegisterResult {
  paired: boolean;
  relay_url: string;
  public_relay_url: string;
  hostname: string;
  agent_machine_id: string;
  link_token: string;
  link_code: string;
}

interface BootstrapDefaults {
  relay_url: string;
  has_bootstrap_token: boolean;
}

function showPair() {
  document.getElementById("view-pair")!.classList.remove("hidden");
  document.getElementById("view-connected")!.classList.add("hidden");
}

function showConnected(status: AgentStatus) {
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
  const errEl = document.getElementById("pair-error")!;
  if (message) {
    errEl.textContent = message;
    errEl.classList.remove("hidden");
  } else {
    errEl.textContent = "";
    errEl.classList.add("hidden");
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
  const codeEl = document.getElementById("manual-code-value")!;
  if (code) {
    codeEl.textContent = code;
  } else {
    codeEl.textContent = "----";
  }
}

async function renderBootstrapQr() {
  setError(null);
  setQrLoading(true, "Preparing secure setup…");

  try {
    const defaults = await invoke<BootstrapDefaults>("get_bootstrap_defaults");
    if (!defaults.has_bootstrap_token) {
      throw new Error("This desktop build is missing its bootstrap token. Reinstall Outpost Agent or use manual pairing below.");
    }

    // Pre-fill relay URL input with current default (saved override or build-time constant)
    const relayInput = document.getElementById("relay-url-input") as HTMLInputElement;
    if (relayInput && !relayInput.value) {
      relayInput.value = defaults.relay_url;
    }

    const result = await invoke<RegisterResult>("bootstrap_register", {
      relayUrl: defaults.relay_url,
      bootstrapToken: "use-baked-token",
    });

    const payload = JSON.stringify({
      relayUrl: result.public_relay_url || result.relay_url,
      linkToken: result.link_token,
      kind: "outpost-phone-link",
    });

    const canvas = document.getElementById("qr-canvas") as HTMLCanvasElement;
    await QRCode.toCanvas(canvas, payload, {
      width: 220,
      margin: 1,
      color: {
        dark: "#18212B",
        light: "#FFFDF9",
      },
    });
    setManualCode(result.link_code);
    setQrLoading(false, "Scan with your iPhone to connect this computer.");
  } catch (e: any) {
    setManualCode(null);
    setQrLoading(true, "QR setup failed. You can still use manual pairing below.");
    setError(String(e?.message ?? e));
  }
}

async function init() {
  const status: AgentStatus = await invoke("get_status");
  if (status.paired) {
    showConnected(status);
    setConnState("connecting");
  } else {
    showPair();
    void renderBootstrapQr();
  }

  await listen<string>("connection_state", (e) => setConnState(e.payload));
  await listen<void>("force_unpair", async () => {
    await invoke("unpair");
    showPair();
    void renderBootstrapQr();
  });
}

document.getElementById("btn-refresh-qr")?.addEventListener("click", () => {
  void renderBootstrapQr();
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
    // Clear the relay URL input so renderBootstrapQr re-fills from the new saved value
    input.value = "";
    void renderBootstrapQr();
  } catch (e: any) {
    setError(String(e?.message ?? e));
  }
});

document.getElementById("btn-toggle-manual")?.addEventListener("click", () => {
  const panel = document.getElementById("manual-panel")!;
  const visible = !panel.classList.contains("hidden");
  panel.classList.toggle("hidden", visible);
  (document.getElementById("btn-toggle-manual") as HTMLButtonElement).textContent = visible
    ? "Use pairing code instead"
    : "Hide pairing code";
});

document.getElementById("btn-unpair")?.addEventListener("click", async () => {
  if (!confirm("Unpair this PC? You'll need to scan again or enter a new code to reconnect.")) return;
  await invoke("unpair");
  showPair();
  void renderBootstrapQr();
});

init();
