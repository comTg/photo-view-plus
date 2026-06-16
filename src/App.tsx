import { RootList } from "@/components/sidebar/RootList";
import { useRoots } from "@/hooks/useRoots";
import { ping } from "@/lib/tauri";
import { open } from "@tauri-apps/plugin-dialog";
import { useCallback, useEffect, useState } from "react";

export default function App() {
  const [profile, setProfile] = useState<string | null>(null);
  const [bootError, setBootError] = useState<string | null>(null);
  const [addError, setAddError] = useState<string | null>(null);

  const roots = useRoots();

  useEffect(() => {
    ping()
      .then(setProfile)
      .catch((e) => setBootError(String(e)));
  }, []);

  const handleAdd = useCallback(async () => {
    setAddError(null);
    try {
      const picked = await open({ directory: true, multiple: false });
      if (!picked || typeof picked !== "string") return;
      await roots.addRoot(picked);
    } catch (e) {
      setAddError(String(e));
    }
  }, [roots]);

  const selectedRoot = roots.roots.find((r) => r.id === roots.selectedId) ?? null;

  return (
    <div className="app-shell">
      <header className="app-header">
        PhotoView+
        <span className="app-header__status">
          {bootError
            ? `后端连接失败：${bootError}`
            : profile
              ? `profile: ${profile}`
              : "正在连接后端…"}
        </span>
      </header>

      <aside className="app-sidebar">
        <RootList
          roots={roots.roots}
          loading={roots.loading}
          error={roots.error}
          selectedId={roots.selectedId}
          onSelect={roots.setSelectedId}
          onRemove={(id) => {
            void roots.removeRoot(id);
          }}
          onAdd={() => {
            void handleAdd();
          }}
        />
        {addError && <div className="rootlist__hint rootlist__hint--error">{addError}</div>}
      </aside>

      <main className="app-main">
        {selectedRoot ? (
          <div className="selected-root">
            <h2 className="selected-root__title">{selectedRoot.label ?? selectedRoot.path}</h2>
            <div className="selected-root__meta">
              <div>路径：{selectedRoot.path}</div>
              <div>类型：{selectedRoot.rootType === "network" ? "网络盘" : "本地盘"}</div>
              <div>状态：{selectedRoot.enabled ? "已启用" : "已禁用"}</div>
              <div>添加于：{new Date(selectedRoot.createdAt * 1000).toLocaleString("zh-CN")}</div>
              <div>
                最近扫描：
                {selectedRoot.lastScanAt === null
                  ? "尚未扫描"
                  : new Date(selectedRoot.lastScanAt * 1000).toLocaleString("zh-CN")}
              </div>
            </div>
            <p className="placeholder-text">扫描入口将在 MVP1 T5 接入</p>
          </div>
        ) : (
          <div className="placeholder-text">
            {roots.roots.length === 0
              ? "左侧点击「+ 添加」选择图片目录开始"
              : "选择左侧的目录查看详情"}
          </div>
        )}
      </main>

      <aside className="app-detail">
        <div className="placeholder-label">详情占位（图片选中后显示）</div>
      </aside>

      <footer className="app-footer">
        共 {roots.roots.length} 个目录
        {roots.selectedId !== null && " · 已选 1 个"}
      </footer>
    </div>
  );
}
