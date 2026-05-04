const { app, BrowserWindow } = require("electron");
const { spawn } = require("node:child_process");
const http = require("node:http");
const path = require("node:path");

const SERVER_URL = process.env.WORKFLOW_SERVER_URL || "http://127.0.0.1:4317";
const RENDERER_URL = process.env.ELECTRON_RENDERER_URL || SERVER_URL;
const EXTERNAL_SERVER = process.env.WORKFLOW_SERVER_EXTERNAL === "1";

let backend = null;

function waitForUrl(url, timeoutMs = 30000) {
  const started = Date.now();
  return new Promise((resolve, reject) => {
    function attempt() {
      const request = http.get(url, (response) => {
        response.resume();
        resolve();
      });
      request.on("error", () => {
        if (Date.now() - started > timeoutMs) {
          reject(new Error(`timed out waiting for ${url}`));
          return;
        }
        setTimeout(attempt, 300);
      });
    }
    attempt();
  });
}

function startBackend() {
  if (EXTERNAL_SERVER) {
    return;
  }

  backend = spawn("cargo", ["run", "--", "workflow", "serve", "--listen", "127.0.0.1:4317"], {
    cwd: path.resolve(__dirname, ".."),
    stdio: "inherit",
    env: process.env,
  });
}

async function createWindow() {
  startBackend();
  await waitForUrl(SERVER_URL);

  const window = new BrowserWindow({
    width: 1480,
    height: 980,
    minWidth: 1180,
    minHeight: 780,
    title: "Workflow Studio",
    backgroundColor: "#0b0d10",
    autoHideMenuBar: true,
    webPreferences: {
      contextIsolation: true,
      sandbox: true,
    },
  });

  await window.loadURL(RENDERER_URL);
}

app.whenReady().then(() => {
  createWindow().catch((error) => {
    console.error(error);
    app.quit();
  });
});

app.on("window-all-closed", () => {
  if (backend) {
    backend.kill("SIGTERM");
    backend = null;
  }
  if (process.platform !== "darwin") {
    app.quit();
  }
});

app.on("before-quit", () => {
  if (backend) {
    backend.kill("SIGTERM");
    backend = null;
  }
});
