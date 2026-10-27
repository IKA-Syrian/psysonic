import { useEffect, useState } from 'react';

// Fork patch: Navidrome answers an artist cover request for an artist without a
// picture with its built-in placeholder (a white star on flat light grey,
// resources/artist-placeholder.webp). Recognise it by its pixels so the
// fullscreen backdrop can use the song's cover instead of a blurred star.

const SIZE = 16;
const results = new Map<string, boolean>();

/** True when a SIZE×SIZE RGBA sample is flat light grey with a white shape. */
export function isArtistPlaceholderPixels(data: Uint8ClampedArray): boolean {
  let white = 0;
  for (let i = 0; i < data.length; i += 4) {
    const r = data[i], g = data[i + 1], b = data[i + 2];
    if (Math.max(r, g, b) - Math.min(r, g, b) > 8 || r < 225) return false;
    if (r >= 248) white++;
  }
  const corners = [0, SIZE - 1, SIZE * (SIZE - 1), SIZE * SIZE - 1];
  if (corners.some(p => data[p * 4] < 228 || data[p * 4] > 242)) return false;
  const share = white / (data.length / 4);
  return share > 0.05 && share < 0.4;
}

function check(url: string): Promise<boolean> {
  return new Promise(resolve => {
    const img = new Image();
    img.onload = () => {
      try {
        const canvas = document.createElement('canvas');
        canvas.width = SIZE;
        canvas.height = SIZE;
        const ctx = canvas.getContext('2d');
        if (!ctx) return resolve(false);
        ctx.drawImage(img, 0, 0, SIZE, SIZE);
        resolve(isArtistPlaceholderPixels(ctx.getImageData(0, 0, SIZE, SIZE).data));
      } catch {
        resolve(false); // cross-origin url: canvas is tainted, assume a real picture
      }
    };
    img.onerror = () => resolve(false);
    img.src = url;
  });
}

/** `null` while the image is being checked, then whether it is the placeholder. */
export function useIsArtistPlaceholder(url: string): boolean | null {
  const [state, setState] = useState<{ url: string; value: boolean | null }>({ url: '', value: null });
  useEffect(() => {
    if (!url || results.has(url)) return;
    let alive = true;
    void check(url).then(value => {
      results.set(url, value);
      if (alive) setState({ url, value });
    });
    return () => { alive = false; };
  }, [url]);
  if (!url) return false;
  return results.get(url) ?? (state.url === url ? state.value : null);
}
