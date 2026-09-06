// Renderer-specific options and ownership stay here. Playback belongs to the viewer.
import AudioMotionAnalyzer from 'audiomotion-analyzer';

const styles = {
  bars: { mode: 6, radial: false, channelLayout: 'single', lineWidth: 0, fillAlpha: 1 },
  line: { mode: 10, radial: false, channelLayout: 'single', lineWidth: 2, fillAlpha: .25 },
  radial: { mode: 6, radial: true, channelLayout: 'single', lineWidth: 0, fillAlpha: 1 },
  stereo: { mode: 6, radial: false, channelLayout: 'dual-horizontal', lineWidth: 0, fillAlpha: 1 },
};

export function createVisualiser(container, { context, source }) {
  const renderer = new AudioMotionAnalyzer(container, {
    audioCtx: context, connectSpeakers: false, start: false, maxFPS: 30,
    showScaleX: false, showPeaks: false,
  });
  let attached = false;
  return {
    style(style, gradient) {
      renderer.setOptions({ ...(styles[style] || styles.bars), gradient: ['classic', 'rainbow', 'steelblue'].includes(gradient) ? gradient : 'classic' });
    },
    pause(paused) {
      if (paused) {
        renderer.stop();
        renderer.disconnectInput();
        attached = false;
      } else {
        if (!attached) { renderer.connectInput(source); attached = true; }
        renderer.start();
      }
    },
    dispose() { renderer.destroy(); },
  };
}
