import { usePlaybackCoverArt } from '@/cover/usePlaybackCoverArt';
import { usePlaybackTrackCoverRef } from '@/cover/useLibraryCoverRef';
import { usePlayerStore } from '@/features/playback';

/**
 * Cover art for the playing track, used only to derive the visualizer palette.
 *
 * Fork patch: follows the playing song's own cover, like the players. A small display size is requested because
 * nothing renders this image — it is decoded once for its dominant colour.
 */
export function useVisualizerCoverArt(): { artUrl: string; artKey: string } {
  const currentTrack = usePlayerStore(s => s.currentTrack);
  const directCover = usePlayerStore(s => s.currentTrack?.directCoverArtUrl);
  const artist = usePlayerStore(s => s.currentTrack?.artist ?? '');
  const album = usePlayerStore(s => s.currentTrack?.album ?? '');
  const coverRef = usePlaybackTrackCoverRef(currentTrack ?? undefined);
  const cover = usePlaybackCoverArt(coverRef, 160, {
    // Palette source should see real art when it exists: arm the chain so a
    // never-opened album resolves instead of sampling the vinyl placeholder.
    ensureOpts: {
      artistName: artist,
      albumTitle: album,
      allowExternalAlbum: true,
    },
  });
  return { artUrl: directCover ?? cover.src, artKey: cover.cacheKey };
}
