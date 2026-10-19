import { useCallback, useState } from 'react';

/**
 * Fork patch: per-album "reversed track order", toggled from the `#` column
 * header. Remembered across visits so a playlist-style folder album (newest
 * track last) can always open newest-first. Play / Add to queue / clicking a
 * row then follow the same order.
 */
const STORAGE_KEY = 'psysonic_fork_album_reversed';

function readReversedIds(): string[] {
  try {
    const parsed: unknown = JSON.parse(localStorage.getItem(STORAGE_KEY) ?? '[]');
    return Array.isArray(parsed) ? parsed.filter((v): v is string => typeof v === 'string') : [];
  } catch {
    return [];
  }
}

export function useAlbumReversedOrder(albumId: string | null | undefined): {
  reversed: boolean;
  toggle: () => void;
} {
  const [ids, setIds] = useState<string[]>(readReversedIds);
  const reversed = !!albumId && ids.includes(albumId);

  const toggle = useCallback(() => {
    if (!albumId) return;
    setIds(prev => {
      const next = prev.includes(albumId) ? prev.filter(id => id !== albumId) : [...prev, albumId];
      try {
        localStorage.setItem(STORAGE_KEY, JSON.stringify(next));
      } catch {
        // Storage unavailable: the toggle still works for this visit.
      }
      return next;
    });
  }, [albumId]);

  return { reversed, toggle };
}
