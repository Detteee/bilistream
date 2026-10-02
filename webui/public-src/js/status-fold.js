// Fold state for the public status accordion.
//
// The fold always holds 服务器 above the platform cards. Idle starts closed.
// 转播中 starts open. A manual toggle sticks until 转播中 changes. A failed
// refresh leaves the last fold alone.

/// @param {{ restreaming: boolean | null, open: boolean }} state
/// @param {{ type: 'status', restreaming: boolean } | { type: 'toggle' } | { type: 'refresh-failed' }} event
/// @returns {{ restreaming: boolean | null, open: boolean }}
export function nextStatusFold(state, event) {
  if (event.type === 'refresh-failed') {
    return state;
  }
  if (event.type === 'toggle') {
    return { restreaming: state.restreaming, open: !state.open };
  }
  if (event.type === 'status') {
    if (event.restreaming === state.restreaming) {
      return state;
    }
    return { restreaming: event.restreaming, open: !!event.restreaming };
  }
  return state;
}
