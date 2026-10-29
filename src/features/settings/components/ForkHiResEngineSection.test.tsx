import { describe, it, expect, beforeEach, vi } from 'vitest';
import { screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { isTauri } from '@tauri-apps/api/core';
import { renderWithProviders } from '@/test/helpers/renderWithProviders';
import { onInvoke, invokeMock } from '@/test/mocks/tauri';
import { ForkHiResEngineSection } from './ForkHiResEngineSection';
import { useForkHiResEnabled, loadForkHiResStatus } from './forkHiResEngineStore';

const status = (over: Record<string, unknown> = {}) => ({
  enabled: true,
  policy: 'seamless',
  ceilingHz: 192000,
  streamRateHz: 192000,
  mixRateHz: 192000,
  lastFormat: null,
  ...over,
});

const setConfigCalls = () => invokeMock.mock.calls.filter(c => c[0] === 'fork_hires_set_config');
const lastConfig = () => setConfigCalls().slice(-1)[0]?.[1]?.config;

beforeEach(() => {
  vi.mocked(isTauri).mockReturnValue(true);
  useForkHiResEnabled.setState({ enabled: false });
  onInvoke('fork_hires_set_config', () => null);
});

describe('ForkHiResEngineSection', () => {
  it('fills its controls from fork_hires_status', async () => {
    onInvoke('fork_hires_status', () => status({ policy: 'adaptive', ceilingHz: 96000, streamRateHz: null, mixRateHz: 96000 }));
    renderWithProviders(<ForkHiResEngineSection />);

    await waitFor(() => expect(screen.getByRole('checkbox', { name: 'Enable hi-res mix engine' })).toBeChecked());
    expect(screen.getByRole('button', { name: 'Adaptive' })).toHaveClass('btn-primary');
    expect(screen.getByRole('button', { name: '96 kHz' })).toHaveClass('btn-primary');
    expect(screen.getByText('Output: — Hz · Windows mix format: 96000 Hz')).toBeInTheDocument();
    expect(useForkHiResEnabled.getState().enabled).toBe(true);
  });

  it('sends the full config when toggling enable', async () => {
    // Stateful like Rust: set_config changes what the next status returns.
    let current = status({ enabled: false });
    onInvoke('fork_hires_status', () => current);
    onInvoke('fork_hires_set_config', (args) => {
      const c = (args as { config: { enabled: boolean } }).config;
      current = status({ enabled: c.enabled });
      return null;
    });
    renderWithProviders(<ForkHiResEngineSection />);
    const toggle = screen.getByRole('checkbox', { name: 'Enable hi-res mix engine' });
    await waitFor(() => expect(invokeMock).toHaveBeenCalledWith('fork_hires_status'));

    await userEvent.click(toggle);

    await waitFor(() => expect(setConfigCalls()).toHaveLength(1));
    expect(lastConfig()).toEqual({
      enabled: true, policy: 'seamless', ceilingHz: null, resyncSilenceMs: 250, chunkFrames: 1024,
    });
    expect(useForkHiResEnabled.getState().enabled).toBe(true);
  });

  it('sends the chosen policy and ceiling (null for 192 kHz)', async () => {
    onInvoke('fork_hires_status', () => status());
    renderWithProviders(<ForkHiResEngineSection />);
    await waitFor(() => expect(screen.getByRole('checkbox', { name: 'Enable hi-res mix engine' })).toBeChecked());

    await userEvent.click(screen.getByRole('button', { name: 'Adaptive' }));
    await waitFor(() => expect(lastConfig()).toMatchObject({ policy: 'adaptive', ceilingHz: null }));

    await userEvent.click(screen.getByRole('button', { name: '384 kHz' }));
    await waitFor(() => expect(lastConfig()).toMatchObject({ policy: 'adaptive', ceilingHz: 384000 }));

    await userEvent.click(screen.getByRole('button', { name: '192 kHz (default)' }));
    await waitFor(() => expect(lastConfig()).toMatchObject({ ceilingHz: null }));
  });

  it('warns only when enabled and the mix rate is below the ceiling', async () => {
    const warning = /Windows is mixing at 48 kHz/;
    onInvoke('fork_hires_status', () => status({ mixRateHz: 48000 }));
    renderWithProviders(<ForkHiResEngineSection />);
    expect(await screen.findByText(warning)).toBeInTheDocument();

    // A 96 kHz ceiling is still above the 48 kHz mix rate, so it still warns.
    await userEvent.click(screen.getByRole('button', { name: '96 kHz' }));
    expect(screen.getByText(warning)).toBeInTheDocument();
  });

  it('does not warn when the mix rate reaches the ceiling or the engine is off', async () => {
    onInvoke('fork_hires_status', () => status({ mixRateHz: 192000 }));
    const first = renderWithProviders(<ForkHiResEngineSection />);
    await waitFor(() => expect(screen.getByRole('checkbox', { name: 'Enable hi-res mix engine' })).toBeChecked());
    expect(screen.queryByText(/Windows is mixing at/)).toBeNull();
    first.unmount();

    onInvoke('fork_hires_status', () => status({ enabled: false, mixRateHz: 48000 }));
    renderWithProviders(<ForkHiResEngineSection />);
    await waitFor(() => expect(screen.getByText(/Windows mix format: 48000 Hz/)).toBeInTheDocument());
    expect(screen.queryByText(/Windows is mixing at/)).toBeNull();
  });

  it('disables the controls outside Tauri', () => {
    vi.mocked(isTauri).mockReturnValue(false);
    renderWithProviders(<ForkHiResEngineSection />);
    expect(screen.getByRole('button', { name: 'Adaptive' })).toBeDisabled();
    expect(invokeMock).not.toHaveBeenCalled();
  });
});

describe('loadForkHiResStatus', () => {
  it('syncs the shared enabled flag', async () => {
    onInvoke('fork_hires_status', () => status());
    await loadForkHiResStatus();
    expect(useForkHiResEnabled.getState().enabled).toBe(true);
  });
});
