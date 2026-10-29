import { describe, it, expect, beforeEach, vi } from 'vitest';
import { act, screen } from '@testing-library/react';
import { renderWithProviders } from '@/test/helpers/renderWithProviders';
import { useAuthStore } from '@/store/authStore';
import { AudioTab } from './AudioTab';
import { useForkHiResEnabled } from './forkHiResEngineStore';

// The device probe talks to Tauri on mount; it is irrelevant here.
vi.mock('@/features/playback/hooks/useAudioDevicesProbe', () => ({
  useAudioDevicesProbe: () => ({
    audioDevices: [], osDefaultAudioDeviceId: null, deviceSwitching: false, devicesLoading: false,
    setDeviceSwitching: () => {}, refreshAudioDevices: async () => {},
  }),
}));

const PICKER = /hi-res.*(blend|crossfade)|crossfade.*rate/i;

beforeEach(() => {
  useAuthStore.setState({ enableHiRes: true });
  useForkHiResEnabled.setState({ enabled: false });
});

describe('AudioTab fork hi-res engine', () => {
  it('shows the legacy blend-rate picker while the fork engine is off', () => {
    renderWithProviders(<AudioTab />);
    expect(screen.getAllByText(PICKER).length).toBeGreaterThan(0);
  });

  it('hides the legacy picker once the fork engine is on, and mounts the fork section', () => {
    useForkHiResEnabled.setState({ enabled: true });
    renderWithProviders(<AudioTab />);
    expect(screen.queryAllByText(PICKER)).toHaveLength(0);
    expect(screen.getByText('Hi-res mix engine (fork)')).toBeInTheDocument();
    act(() => useForkHiResEnabled.setState({ enabled: false }));
    expect(screen.getAllByText(PICKER).length).toBeGreaterThan(0);
  });
});
