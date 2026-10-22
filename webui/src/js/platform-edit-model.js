// platform-edit-model.js — the three-platform inline editors' shape, without a DOM.
//
// overview.js drives the editors (open, fill, save, echo); this module only says
// which rows exist, which element ids each row owns, and how a selected option
// becomes a request body. Keeping it DOM-free lets the tests pin the id contract
// against index.html and the payload shape against the API.

const PLATFORM_EDIT_PREFIXES = [
  { prefix: 'yt', platform: 'youtube', label: 'YouTube' },
  { prefix: 'tw', platform: 'twitch', label: 'Twitch' },
  { prefix: 'nc', platform: 'niconico', label: 'Niconico' }
];

const PLATFORM_EDIT_KINDS = ['channel', 'area', 'quality'];

const PLATFORM_EDIT_ROWS = PLATFORM_EDIT_PREFIXES.flatMap((platform) =>
  PLATFORM_EDIT_KINDS.map((kind) => ({ ...platform, kind }))
);

// The channel row's value span is `-channel-name`; area and quality use the kind
// itself. Everything else follows `<prefix>-<kind>-...`.
function fieldEditIds(row) {
  return {
    span: row.kind === 'channel' ? `${row.prefix}-channel-name` : `${row.prefix}-${row.kind}`,
    container: `${row.prefix}-${row.kind}-edit-container`,
    select: `${row.prefix}-${row.kind}-edit-select`,
    editButton: `${row.prefix}-${row.kind}-edit-btn`,
    saveButton: `${row.prefix}-${row.kind}-save-btn`,
    cancelButton: `${row.prefix}-${row.kind}-cancel-btn`
  };
}

// Channel options carry the whole candidate as JSON in their value.
function channelEditPayload(optionValue) {
  const info = JSON.parse(optionValue);
  return { channel_id: info.id, channel_name: info.name };
}

export { PLATFORM_EDIT_KINDS, PLATFORM_EDIT_PREFIXES, PLATFORM_EDIT_ROWS, channelEditPayload, fieldEditIds };
