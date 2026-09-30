// Pure placement state for the public 服务器 card.
//
// The card starts above the platform cards. Once status arrives, it stays
// above while the active node is 转播中 and moves below 直播与预告 otherwise.
// Failed refreshes leave the last known placement alone.

/// @param {{ placement: 'above' | 'below', restreaming: boolean | null }} state
/// @param {{ type: 'status', restreaming: boolean } | { type: 'refresh-failed' }} event
/// @returns {{ placement: 'above' | 'below', restreaming: boolean | null }}
export function nextServerCardPlacement(state, event) {
  if (event.type === 'refresh-failed') {
    return state;
  }
  if (event.type === 'status') {
    if (event.restreaming === state.restreaming) {
      return state;
    }
    return {
      placement: event.restreaming ? 'above' : 'below',
      restreaming: event.restreaming,
    };
  }
  return state;
}
