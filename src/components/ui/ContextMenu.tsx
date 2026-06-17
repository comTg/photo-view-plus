import { type MouseEvent, useEffect } from "react";

export interface ContextMenuItem {
  id: string;
  label: string;
  disabled?: boolean;
  onSelect: () => void;
}

export interface ContextMenuState {
  x: number;
  y: number;
}

interface ContextMenuProps {
  menu: ContextMenuState | null;
  items: ContextMenuItem[];
  onClose: () => void;
}

export function ContextMenu({ menu, items, onClose }: ContextMenuProps) {
  useEffect(() => {
    if (!menu) return;

    const close = () => onClose();
    const closeOnEscape = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };

    window.addEventListener("pointerdown", close);
    window.addEventListener("scroll", close, true);
    window.addEventListener("keydown", closeOnEscape);
    window.addEventListener("resize", close);
    return () => {
      window.removeEventListener("pointerdown", close);
      window.removeEventListener("scroll", close, true);
      window.removeEventListener("keydown", closeOnEscape);
      window.removeEventListener("resize", close);
    };
  }, [menu, onClose]);

  if (!menu) return null;

  return (
    <div
      className="context-menu"
      style={{ left: menu.x, top: menu.y }}
      onContextMenu={(event) => event.preventDefault()}
      onPointerDown={(event) => event.stopPropagation()}
      role="menu"
    >
      {items.map((item) => (
        <button
          key={item.id}
          type="button"
          disabled={item.disabled}
          onClick={() => {
            item.onSelect();
            onClose();
          }}
          role="menuitem"
        >
          {item.label}
        </button>
      ))}
    </div>
  );
}

export function menuPosition(event: MouseEvent, width = 220, height = 96): ContextMenuState {
  return {
    x: Math.min(event.clientX, Math.max(0, window.innerWidth - width - 8)),
    y: Math.min(event.clientY, Math.max(0, window.innerHeight - height - 8)),
  };
}
