-- docs/11 R4：缩略图从无损 WebP 改成有损 JPEG。
--
-- 记下每张图缩略图文件的扩展名：已有的缩略图都是 webp（默认值），新生成的写 jpg。
-- 缩略图协议按它设置 Content-Type、拼文件路径；空闲时后台把旧的 webp 逐批转成 jpg，转完删掉旧文件。

ALTER TABLE images ADD COLUMN thumb_ext TEXT NOT NULL DEFAULT 'webp';

-- 后台转换每批查"还是 webp 的已就绪缩略图"。转完以后这个部分索引就是空的，不占地方。
CREATE INDEX IF NOT EXISTS idx_images_thumb_webp
    ON images(id) WHERE thumb_ext = 'webp' AND thumb_status = 'ready';
