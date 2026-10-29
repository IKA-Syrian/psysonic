import { create } from 'zustand';
import { isTauri } from '@tauri-apps/api/core';
import { commands } from '@/generated/bindings';
import type { ForkHiResStatus } from '@/generated/bindings';

// Fork patch: shared state for the hi-res mix engine settings (Rust is the source of truth).

/** Whether the fork engine is on, mirrored from Rust so AudioTab can react to it. */
export const useForkHiResEnabled = create<{ enabled: boolean; setEnabled: (v: boolean) => void }>()(set => ({
  enabled: false,
  setEnabled: enabled => set({ enabled }),
}));

/** Reads the engine status from Rust and syncs the shared enabled flag. Null if unavailable. */
export async function loadForkHiResStatus(): Promise<ForkHiResStatus | null> {
  if (!isTauri()) return null;
  try {
    const status = await commands.forkHiresStatus();
    useForkHiResEnabled.getState().setEnabled(status.config.enabled);
    return status;
  } catch {
    return null;
  }
}
