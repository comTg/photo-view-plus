import { originalUrl } from "@/lib/tauri";
import type { ImageRecord } from "@/lib/tauri-types";
import Lightbox, { type Slide } from "yet-another-react-lightbox";
import Counter from "yet-another-react-lightbox/plugins/counter";
import Fullscreen from "yet-another-react-lightbox/plugins/fullscreen";
import Zoom from "yet-another-react-lightbox/plugins/zoom";

interface ImagePreviewLightboxProps {
  images: ImageRecord[];
  index: number;
  open: boolean;
  onClose: () => void;
  onIndexChange: (index: number) => void;
}

export function ImagePreviewLightbox({
  images,
  index,
  open,
  onClose,
  onIndexChange,
}: ImagePreviewLightboxProps) {
  const slides: Slide[] = images.map((image) => ({
    src: originalUrl(image.id),
    alt: image.filename,
    width: image.width ?? undefined,
    height: image.height ?? undefined,
    imageFit: "contain",
  }));

  return (
    <Lightbox
      open={open && slides.length > 0}
      close={onClose}
      slides={slides}
      index={Math.min(index, Math.max(0, slides.length - 1))}
      plugins={[Counter, Fullscreen, Zoom]}
      carousel={{
        finite: true,
        imageFit: "contain",
        preload: 1,
      }}
      controller={{ closeOnBackdropClick: true }}
      on={{
        view: ({ index: currentIndex }) => onIndexChange(currentIndex),
      }}
      zoom={{
        maxZoomPixelRatio: 1.5,
        scrollToZoom: true,
        wheelZoomDistanceFactor: 120,
      }}
      labels={{
        Close: "关闭",
        Next: "下一张",
        Previous: "上一张",
        "Enter Fullscreen": "进入全屏",
        "Exit Fullscreen": "退出全屏",
        "Zoom in": "放大",
        "Zoom out": "缩小",
        "{index} of {total}": "{index} / {total}",
      }}
      styles={{
        container: { backgroundColor: "rgba(8, 12, 18, 0.94)" },
      }}
    />
  );
}
