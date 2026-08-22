import React, { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { Check, Copy, RefreshCcw } from "lucide-react";
import { commands, type RemoteModel, type ServerStatus } from "@/bindings";

import { Alert } from "../../ui/Alert";
import { Button } from "../../ui/Button";
import { Input } from "../../ui/Input";
import { Select } from "../../ui/Select";
import { SettingContainer, SettingsGroup } from "@/components/ui";
import { ToggleSwitch } from "../../ui/ToggleSwitch";
import { useSettings } from "../../../hooks/useSettings";

/** Outcome of a "test connection" attempt, rendered inline under the fields. */
type ProbeResult =
  | { state: "idle" }
  | { state: "testing" }
  | {
      state: "ok";
      server: string;
      streaming: boolean;
      loadedModel: string | null;
    }
  | { state: "error"; message: string };

/**
 * Server half of the panel: run this machine's engine as a network service.
 *
 * The token and the reachable URLs live in backend state rather than settings
 * (the bound address is only known once the listener starts), so this polls
 * `getServerStatus` after every change that could restart it.
 */
const ServerSection: React.FC = () => {
  const { t } = useTranslation();
  const { getSetting, updateSetting, isUpdating } = useSettings();
  const [status, setStatus] = useState<ServerStatus | null>(null);
  const [copied, setCopied] = useState<string | null>(null);

  const enabled = getSetting("server_enabled") ?? false;
  const port = getSetting("server_port") ?? 8756;
  const exposeLan = getSetting("server_expose_lan") ?? true;

  const refreshStatus = useCallback(async () => {
    const result = await commands.getServerStatus();
    setStatus(result.status === "ok" ? result.data : null);
  }, []);

  useEffect(() => {
    void refreshStatus();
  }, [refreshStatus, enabled, port, exposeLan]);

  const copy = async (value: string) => {
    await navigator.clipboard.writeText(value);
    setCopied(value);
    // Revert the check mark so the button reads as reusable.
    window.setTimeout(() => setCopied(null), 1500);
  };

  const regenerate = async () => {
    await commands.regenerateServerToken();
    await refreshStatus();
  };

  return (
    <SettingsGroup
      title={t("settings.network.server.title")}
      description={t("settings.network.server.description")}
    >
      <ToggleSwitch
        checked={enabled}
        onChange={(value) => updateSetting("server_enabled", value)}
        isUpdating={isUpdating("server_enabled")}
        label={t("settings.network.server.enable.label")}
        description={t("settings.network.server.enable.description")}
        grouped
      />

      {enabled && (
        <>
          <SettingContainer
            title={t("settings.network.server.port.title")}
            description={t("settings.network.server.port.description")}
            grouped
          >
            <Input
              variant="compact"
              type="number"
              min={1024}
              max={65535}
              className="w-24"
              value={port}
              onChange={(event) => {
                const next = Number(event.target.value);
                if (Number.isInteger(next) && next >= 1024 && next <= 65535) {
                  void updateSetting("server_port", next);
                }
              }}
            />
          </SettingContainer>

          <ToggleSwitch
            checked={exposeLan}
            onChange={(value) => updateSetting("server_expose_lan", value)}
            isUpdating={isUpdating("server_expose_lan")}
            label={t("settings.network.server.exposeLan.label")}
            description={t("settings.network.server.exposeLan.description")}
            grouped
          />

          <SettingContainer
            title={t("settings.network.server.token.title")}
            description={t("settings.network.server.token.description")}
            grouped
            layout="stacked"
          >
            <div className="flex items-center gap-2">
              <Input
                variant="compact"
                readOnly
                className="flex-1 font-mono text-xs"
                value={status?.token ?? ""}
              />
              <Button
                variant="secondary"
                size="sm"
                onClick={() => void copy(status?.token ?? "")}
                title={t("settings.network.copy")}
              >
                {copied === status?.token ? (
                  <Check size={14} />
                ) : (
                  <Copy size={14} />
                )}
              </Button>
              <Button
                variant="secondary"
                size="sm"
                onClick={() => void regenerate()}
              >
                <RefreshCcw size={14} />
              </Button>
            </div>
          </SettingContainer>

          <SettingContainer
            title={t("settings.network.server.address.title")}
            description={t("settings.network.server.address.description")}
            grouped
            layout="stacked"
          >
            {status?.running ? (
              <div className="flex flex-col gap-1">
                {status.client_urls.map((url) => (
                  <div key={url} className="flex items-center gap-2">
                    <code className="flex-1 text-xs font-mono text-mid-gray">
                      {url}
                    </code>
                    <Button
                      variant="ghost"
                      size="sm"
                      onClick={() => void copy(url)}
                      title={t("settings.network.copy")}
                    >
                      {copied === url ? (
                        <Check size={14} />
                      ) : (
                        <Copy size={14} />
                      )}
                    </Button>
                  </div>
                ))}
              </div>
            ) : (
              <Alert variant="error" contained>
                {t("settings.network.server.notRunning")}
              </Alert>
            )}
          </SettingContainer>
        </>
      )}
    </SettingsGroup>
  );
};

/**
 * Client half: point this machine at someone else's engine.
 *
 * Address and token are edited locally and saved on blur rather than per
 * keystroke, so a half-typed IP is never persisted or probed.
 */
const ClientSection: React.FC = () => {
  const { t } = useTranslation();
  const { settings, getSetting, updateSetting, isUpdating } = useSettings();

  const [baseUrl, setBaseUrl] = useState("");
  const [token, setToken] = useState("");
  const [probe, setProbe] = useState<ProbeResult>({ state: "idle" });
  const [models, setModels] = useState<RemoteModel[]>([]);

  // Adopt the persisted values once settings arrive, without clobbering an edit
  // in progress on later store updates.
  useEffect(() => {
    if (!settings) return;
    setBaseUrl((current) => current || (settings.client_base_url ?? ""));
    setToken((current) => current || (settings.client_token ?? ""));
  }, [settings]);

  const selectedModel = getSetting("client_model") ?? "";
  const streaming = getSetting("client_streaming") ?? true;
  const fallbackLocal = getSetting("client_fallback_local") ?? false;
  const timeout = getSetting("client_timeout_secs") ?? 120;

  const saveConnection = async () => {
    await commands.changeClientConnection(baseUrl, token);
  };

  const testConnection = async () => {
    setProbe({ state: "testing" });
    await saveConnection();

    const result = await commands.testServerConnection(baseUrl, token);
    if (result.status !== "ok") {
      setProbe({ state: "error", message: result.error });
      setModels([]);
      return;
    }

    // Every field of the info document is optional: a generic
    // OpenAI-compatible server answers with none of them, and even a Handy
    // server may omit some. Absent means "not advertised", never "unusable".
    setProbe({
      state: "ok",
      server: result.data.server || t("settings.network.client.genericServer"),
      streaming: result.data.streaming ?? false,
      loadedModel: result.data.loaded_model ?? null,
    });

    // A failed model list is not a failed connection: a server can transcribe
    // without exposing a usable list, so the picker just stays empty.
    const list = await commands.listServerModels(baseUrl, token);
    setModels(list.status === "ok" ? list.data : []);
  };

  return (
    <SettingsGroup
      title={t("settings.network.client.title")}
      description={t("settings.network.client.description")}
    >
      <SettingContainer
        title={t("settings.network.client.address.title")}
        description={t("settings.network.client.address.description")}
        grouped
        layout="stacked"
      >
        <Input
          variant="compact"
          className="w-full"
          placeholder="192.168.1.20:8756"
          value={baseUrl}
          onChange={(event) => setBaseUrl(event.target.value)}
          onBlur={() => void saveConnection()}
        />
      </SettingContainer>

      <SettingContainer
        title={t("settings.network.client.token.title")}
        description={t("settings.network.client.token.description")}
        grouped
        layout="stacked"
      >
        <div className="flex items-center gap-2">
          <Input
            variant="compact"
            type="password"
            className="flex-1 font-mono text-xs"
            value={token}
            onChange={(event) => setToken(event.target.value)}
            onBlur={() => void saveConnection()}
          />
          <Button
            variant="secondary"
            size="sm"
            disabled={probe.state === "testing" || !baseUrl.trim()}
            onClick={() => void testConnection()}
          >
            {t("settings.network.client.test")}
          </Button>
        </div>
      </SettingContainer>

      {probe.state === "ok" && (
        <Alert variant="success" contained>
          {t("settings.network.client.connected", {
            server: probe.server,
            model:
              probe.loadedModel ?? t("settings.network.client.serverDefault"),
          })}
          {!probe.streaming && ` — ${t("settings.network.client.noStreaming")}`}
        </Alert>
      )}
      {probe.state === "error" && (
        <Alert variant="error" contained>
          {probe.message}
        </Alert>
      )}

      <SettingContainer
        title={t("settings.network.client.model.title")}
        description={t("settings.network.client.model.description")}
        grouped
      >
        <Select
          value={selectedModel || null}
          isClearable
          placeholder={t("settings.network.client.serverDefault")}
          options={models.map((model) => ({
            value: model.id,
            label: model.name ?? model.id,
          }))}
          onChange={(value) => void updateSetting("client_model", value ?? "")}
        />
      </SettingContainer>

      <ToggleSwitch
        checked={streaming}
        onChange={(value) => updateSetting("client_streaming", value)}
        isUpdating={isUpdating("client_streaming")}
        label={t("settings.network.client.streaming.label")}
        description={t("settings.network.client.streaming.description")}
        grouped
      />

      <ToggleSwitch
        checked={fallbackLocal}
        onChange={(value) => updateSetting("client_fallback_local", value)}
        isUpdating={isUpdating("client_fallback_local")}
        label={t("settings.network.client.fallback.label")}
        description={t("settings.network.client.fallback.description")}
        grouped
      />

      <SettingContainer
        title={t("settings.network.client.timeout.title")}
        description={t("settings.network.client.timeout.description")}
        grouped
      >
        <Input
          variant="compact"
          type="number"
          min={1}
          max={3600}
          className="w-24"
          value={timeout}
          onChange={(event) => {
            const next = Number(event.target.value);
            if (Number.isInteger(next) && next >= 1 && next <= 3600) {
              void updateSetting("client_timeout_secs", next);
            }
          }}
        />
      </SettingContainer>
    </SettingsGroup>
  );
};

/**
 * Networked inference: run transcription on another machine, or offer this
 * machine's engine to others.
 *
 * The two halves are independent on purpose — a desktop with a GPU typically
 * dictates locally *and* serves, so serving is a toggle rather than a mode.
 */
export const NetworkSettings: React.FC = () => {
  const { t } = useTranslation();
  const { getSetting, updateSetting } = useSettings();

  const mode = getSetting("inference_mode") ?? "local";

  return (
    <div className="space-y-6">
      <SettingsGroup title={t("settings.network.mode.title")}>
        <SettingContainer
          title={t("settings.network.mode.label")}
          description={t("settings.network.mode.description")}
          grouped
        >
          <Select
            value={mode}
            options={[
              { value: "local", label: t("settings.network.mode.local") },
              { value: "client", label: t("settings.network.mode.client") },
            ]}
            onChange={(value) =>
              void updateSetting(
                "inference_mode",
                (value ?? "local") as "local" | "client",
              )
            }
          />
        </SettingContainer>
      </SettingsGroup>

      {mode === "client" && <ClientSection />}
      <ServerSection />
    </div>
  );
};
