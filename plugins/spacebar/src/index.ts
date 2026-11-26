// @description Fixes Spacebar payloads that Phoenix cannot decode
// @version 0.2.0
// @author softgaypaws
// @unlisted

const seen = new Set();
const decorations = phoenix.storage.get("decorations") ?? {};

function note(path) {
  if (seen.has(path)) return;
  seen.add(path);
  phoenix.log.info(`spacebar: fixing ${path}`);
}

function fixObject(object) {
  let changed = false;
  const decoration = object.avatar_decoration_data;
  if (decoration && typeof decoration === "object" && typeof decoration.asset === "string") {
    const hex = decoration.asset.replace(/^a_/, "");
    if (!/^\d+$/.test(decoration.sku_id ?? "") && /^[0-9a-f]{15}/.test(hex)) {
      decoration.sku_id = BigInt(`0x${hex.slice(0, 15)}`).toString();
      note("avatar_decoration_data.sku_id");
      changed = true;
    }
    if (/^\d+$/.test(decoration.sku_id ?? "") && decorations[decoration.sku_id] !== decoration.asset) {
      decorations[decoration.sku_id] = decoration.asset;
      phoenix.storage.set("decorations", decorations);
    }
  }
  if ("is_spam_request" in object || ("type" in object && "nickname" in object && "user" in object)) {
    if (typeof object.is_spam_request !== "boolean") {
      object.is_spam_request = false;
      note("relationship.is_spam_request");
      changed = true;
    }
  }
  if (Array.isArray(object.recipients) && "type" in object && object.last_message_id === null) {
    object.last_message_id = object.id;
    note("channel.last_message_id");
    changed = true;
  }
  if (object.properties && typeof object.properties === "object" && typeof object.version === "string") {
    object.version = Number(object.version) || 0;
    note("guild.version");
    changed = true;
  }
  if ("session_id" in object && "self_mute" in object) {
    for (const key of ["deaf", "mute", "self_deaf", "self_mute", "self_video", "self_stream", "suppress"]) {
      if (key in object && typeof object[key] !== "boolean") {
        object[key] = false;
        note(`voice_state.${key}`);
        changed = true;
      }
    }
  }
  if ("premium_progress_bar_enabled" in object && typeof object.premium_progress_bar_enabled !== "boolean") {
    object.premium_progress_bar_enabled = false;
    note("guild.premium_progress_bar_enabled");
    changed = true;
  }
  return changed;
}

function fixDeep(value) {
  if (Array.isArray(value)) {
    let changed = false;
    for (const item of value) changed = fixDeep(item) || changed;
    return changed;
  }
  if (!value || typeof value !== "object") return false;
  let changed = fixObject(value);
  for (const key in value) changed = fixDeep(value[key]) || changed;
  return changed;
}

for (const name of Object.keys(phoenix.gateway)) {
  const sig = phoenix.gateway[name];
  if (!sig) continue;
  phoenix.hook(sig, {
    before(payload) {
      if (fixDeep(payload)) return payload;
    },
  });
}
