import { type Event, listen } from "@tauri-apps/api/event";
import { useEffect } from "react";

export function useTauriEvent<T>(eventName: string, handler: (payload: T) => void) {
  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | undefined;

    listen<T>(eventName, (event: Event<T>) => {
      handler(event.payload);
    })
      .then((dispose) => {
        if (disposed) {
          dispose();
        } else {
          unlisten = dispose;
        }
      })
      .catch((error) => {
        console.warn(`订阅事件失败：${eventName}`, error);
      });

    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [eventName, handler]);
}
