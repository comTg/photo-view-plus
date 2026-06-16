import { ping } from "@/lib/tauri";
import { useEffect, useState } from "react";

export default function App() {
  const [profile, setProfile] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    ping()
      .then(setProfile)
      .catch((e) => setError(String(e)));
  }, []);

  return (
    <div className="app-shell">
      <header className="app-header">
        PhotoView+
        <span className="app-header__status">
          {error ? `后端连接失败：${error}` : profile ? `profile: ${profile}` : "正在连接后端…"}
        </span>
      </header>

      <aside className="app-sidebar">
        <div className="placeholder-label">导航占位</div>
        <div>我的相册</div>
        <div>文件夹</div>
        <div>标签</div>
        <div>智能相册</div>
      </aside>

      <main className="app-main">
        <div className="placeholder-text">中央内容区占位（MVP1 T11 将替换为瀑布流）</div>
      </main>

      <aside className="app-detail">
        <div className="placeholder-label">详情占位</div>
      </aside>

      <footer className="app-footer">状态栏占位</footer>
    </div>
  );
}
