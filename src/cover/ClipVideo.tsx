import { useEffect, useRef, useState } from 'react';
import { usePlayerStore } from '@/features/playback';

/**
 * Fork patch: a muted looping clip laid over a cover or background (see
 * songClips.ts). It fades in once it actually plays, so a missing file just
 * leaves the picture underneath, and it pauses while the music is paused.
 */
export function ClipVideo({ src, className = '' }: { src: string; className?: string }) {
  const ref = useRef<HTMLVideoElement>(null);
  const [state, setState] = useState<{ src: string; playing: boolean }>({ src: '', playing: false });
  const isPlaying = usePlayerStore(s => s.isPlaying);

  useEffect(() => {
    const v = ref.current;
    if (!v) return;
    if (isPlaying) void v.play().catch(() => {});
    else v.pause();
  }, [isPlaying, src]);

  if (!src) return null;
  const shown = state.src === src && state.playing;
  return (
    <video
      key={src}
      ref={ref}
      className={`fork-clip${shown ? ' is-playing' : ''} ${className}`}
      src={src}
      autoPlay={isPlaying}
      muted
      loop
      playsInline
      aria-hidden="true"
      onPlaying={() => setState({ src, playing: true })}
      onError={() => setState({ src, playing: false })}
    />
  );
}

/**
 * Fullscreen background clip. Spotify canvases are 9:16, so the clip is shown
 * whole at full height, over a blurred copy of itself that fills the screen.
 */
export function ClipBackdrop({ src }: { src: string }) {
  if (!src) return null;
  return (
    <div className="fork-clip-backdrop" aria-hidden="true">
      <ClipVideo src={src} className="fork-clip-backdrop-fill" />
      <ClipVideo src={src} className="fork-clip-backdrop-main" />
    </div>
  );
}
