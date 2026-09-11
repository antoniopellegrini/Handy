import React, { useState, useEffect } from "react";
import { getVersion } from "@tauri-apps/api/app";
import { Network } from "lucide-react";
import { useTranslation } from "react-i18next";

import ModelSelector from "../model-selector";
import UpdateChecker from "../update-checker";
import { useSettingsStore } from "@/stores/settingsStore";

interface FooterProps {
  onNetworkClick: () => void;
}

const Footer: React.FC<FooterProps> = ({ onNetworkClick }) => {
  const { t } = useTranslation();
  const [version, setVersion] = useState("");
  const usesRemoteInference = useSettingsStore(
    (state) => state.settings?.inference_mode === "client",
  );

  useEffect(() => {
    const fetchVersion = async () => {
      try {
        const appVersion = await getVersion();
        setVersion(appVersion);
      } catch (error) {
        console.error("Failed to get app version:", error);
        setVersion("0.1.2");
      }
    };

    fetchVersion();
  }, []);

  return (
    <div className="w-full border-t border-mid-gray/20 pt-3">
      <div className="flex justify-between items-center text-xs px-4 pb-3 text-text/60">
        <div className="flex items-center gap-4">
          {usesRemoteInference ? (
            <button
              type="button"
              onClick={onNetworkClick}
              className="flex items-center gap-2 transition-colors hover:text-text/80"
              title={t("footer.remoteServer")}
            >
              <Network className="h-4 w-4 text-logo-primary" />
              <span>{t("footer.remoteServer")}</span>
            </button>
          ) : (
            <ModelSelector />
          )}
        </div>

        {/* Update Status */}
        <div className="flex items-center gap-1">
          <UpdateChecker />
          <span>•</span>
          {/* eslint-disable-next-line i18next/no-literal-string */}
          <span>v{version}</span>
        </div>
      </div>
    </div>
  );
};

export default Footer;
