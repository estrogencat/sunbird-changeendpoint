// @description Fixes Spacebar payloads that Phoenix cannot decode
// @version 0.3.0
// @author softgaypaws
// @unlisted

const seen = new Set();
const decorations = phoenix.storage.get("decorations") ?? {};
const listIds = new Map();
const colorRoles = new Map();
const listOverwrites = new Map();
const guildRoles = new Map();
const guildOwners = new Map();
const missingMembers = new Set();
const fetchedMembers = new Set();
let fetchingMembers = false;
const COLOR_BIT = 1n << 62n;
const FIXABLE = /avatar_decoration_data|is_spam_request|last_message_id|premium_progress_bar_enabled|self_mute|"version"|"roles"|"joined_at"|"video\//;

function note(path) {
  if (seen.has(path)) return;
  seen.add(path);
  phoenix.log.info(`spacebar: fixing ${path}`);
}

function murmur(key) {
  let h = 0;
  let i = 0;
  let k;
  const rem = key.length & 3;
  const bytes = key.length - rem;
  const mix = value => {
    value = Math.imul(value, 0xcc9e2d51);
    value = (value << 15) | (value >>> 17);
    return Math.imul(value, 0x1b873593);
  };
  while (i < bytes) {
    k = (key.charCodeAt(i) & 0xff) | ((key.charCodeAt(i + 1) & 0xff) << 8) | ((key.charCodeAt(i + 2) & 0xff) << 16) | ((key.charCodeAt(i + 3) & 0xff) << 24);
    i += 4;
    h ^= mix(k);
    h = (h << 13) | (h >>> 19);
    h = (Math.imul(h, 5) + 0xe6546b64) | 0;
  }
  k = 0;
  if (rem === 3) k ^= (key.charCodeAt(i + 2) & 0xff) << 16;
  if (rem >= 2) k ^= (key.charCodeAt(i + 1) & 0xff) << 8;
  if (rem >= 1) h ^= mix(k ^ (key.charCodeAt(i) & 0xff));
  h ^= key.length;
  h ^= h >>> 16;
  h = Math.imul(h, 0x85ebca6b);
  h ^= h >>> 13;
  h = Math.imul(h, 0xc2b2ae35);
  h ^= h >>> 16;
  return (h >>> 0).toString();
}

function listIdOf(channel) {
  const perms = [];
  for (const overwrite of channel.permission_overwrites ?? []) {
    if (BigInt(overwrite.allow ?? 0) & 1024n) perms.push(`allow:${overwrite.id}`);
    else if (BigInt(overwrite.deny ?? 0) & 1024n) perms.push(`deny:${overwrite.id}`);
  }
  return perms.length ? murmur(perms.sort().join(",")) : "everyone";
}

function rememberList(guildId, channel) {
  if (!Array.isArray(channel.permission_overwrites)) return;
  if (!listIds.has(guildId)) listIds.set(guildId, new Set(["everyone"]));
  const id = listIdOf(channel);
  listIds.get(guildId).add(id);
  listOverwrites.set(`${guildId}:${id}`, channel.permission_overwrites);
}

function canView(guildId, overwrites, userId, roles) {
  if (guildOwners.get(guildId) === userId) return true;
  const perms = guildRoles.get(guildId);
  if (!perms) return true;
  let bits = perms.get(guildId) ?? 0n;
  for (const role of roles) bits |= perms.get(role) ?? 0n;
  if (bits & 8n) return true;
  const apply = (allow, deny) => {
    bits = (bits & ~deny) | allow;
  };
  const everyone = overwrites.find(o => o.id === guildId);
  if (everyone) apply(BigInt(everyone.allow ?? 0), BigInt(everyone.deny ?? 0));
  let allow = 0n;
  let deny = 0n;
  for (const o of overwrites) {
    if (o.id !== guildId && roles.includes(o.id)) {
      allow |= BigInt(o.allow ?? 0);
      deny |= BigInt(o.deny ?? 0);
    }
  }
  apply(allow, deny);
  const own = overwrites.find(o => o.id === userId);
  if (own) apply(BigInt(own.allow ?? 0), BigInt(own.deny ?? 0));
  return (bits & 1024n) !== 0n;
}

function listFor(payload, id) {
  const overwrites = listOverwrites.get(`${payload.guild_id}:${id}`) ?? [];
  const counts = new Map();
  const ops = (payload.ops ?? []).map(op => {
    if (op.op !== "SYNC" || !op.items || !op.range) return op;
    let group = null;
    const kept = op.items.filter(item => {
      if (item.group) {
        group = item.group.id;
        counts.set(group, 0);
        return true;
      }
      const member = item.member;
      if (member && !canView(payload.guild_id, overwrites, member.user?.id, member.roles ?? [])) return false;
      if (group) counts.set(group, (counts.get(group) ?? 0) + 1);
      return true;
    });
    const items = kept.filter(item => !item.group || counts.get(item.group.id)).map(item => item.group ? { group: { ...item.group, count: counts.get(item.group.id) ?? 0 } } : item);
    return { ...op, items, range: [op.range[0], op.range[0] + Math.max(items.length - 1, 0)] };
  });
  const groups = counts.size && payload.groups ? payload.groups.filter(g => counts.get(g.id)).map(g => ({ ...g, count: counts.get(g.id) ?? 0 })) : payload.groups;
  return { ...payload, id, ops, groups };
}

async function fetchMissingMembers() {
  if (fetchingMembers) return;
  fetchingMembers = true;
  for (const key of missingMembers) {
    missingMembers.delete(key);
    const [guildId, userId] = key.split(":");
    const res = await phoenix.discord.get(`/guilds/${guildId}/members/${userId}`).catch(() => null);
    const member = res?.ok ? res.json() : null;
    if (!member) continue;
    fixDeep(member);
    phoenix.enqueue(phoenix.gateway.guildMembersChunk, { guild_id: guildId, members: [member], not_found: [], chunk_index: 0, chunk_count: 1 });
  }
  fetchingMembers = false;
}

function fixGuild(guild) {
  guildRoles.set(guild.id, new Map(guild.roles.filter(Boolean).map(role => [role.id, BigInt(role.permissions ?? 0)])));
  const owner = guild.owner_id ?? guild.properties?.owner_id;
  if (owner) guildOwners.set(guild.id, owner);
  if (Array.isArray(guild.channels)) for (const channel of guild.channels) rememberList(guild.id, channel);
  const everyone = guild.roles.find(role => role?.id === guild.id);
  if (!everyone || !rememberColor(guild.id, everyone) || guild.roles.some(role => role?.id === colorRoles.get(guild.id))) return false;
  guild.roles.push(colorRoleOf(everyone));
  note("guild.everyone_color");
  return true;
}

function colorRoleOf(role) {
  return { ...role, id: (BigInt(role.id) | COLOR_BIT).toString(), hoist: false, mentionable: false, managed: false, permissions: "0", position: 1 };
}

function rememberColor(guildId, everyone) {
  if (everyone.color) colorRoles.set(guildId, colorRoleOf(everyone).id);
  else colorRoles.delete(guildId);
  return Boolean(everyone.color);
}

function fixObject(object) {
  let changed = false;
  if (typeof object.id === "string" && Array.isArray(object.roles) && typeof object.roles[0] === "object") changed = fixGuild(object) || changed;
  if (typeof object.guild_id === "string" && typeof object.id === "string") rememberList(object.guild_id, object);
  const memberGuild = "joined_at" in object && Array.isArray(object.roles) && (typeof object.guild_id === "string" ? object.guild_id : object.roles.find(id => colorRoles.has(id)));
  const colorRole = memberGuild && colorRoles.get(memberGuild);
  if (colorRole && !object.roles.includes(colorRole)) {
    object.roles = [...object.roles.filter(id => id !== memberGuild), colorRole];
    note("member.everyone_color");
    changed = true;
  }
  if (typeof object.content_type === "string" && object.content_type.startsWith("video/") && "filename" in object && (!object.width || !object.height)) {
    object.width = 640;
    object.height = 360;
    note("attachment.video_size");
    changed = true;
  }
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
      const changed = fixDeep(payload);
      if (name === "guildMemberListUpdate") {
        for (const id of listIds.get(payload.guild_id) ?? []) if (id !== payload.id) phoenix.dispatch(sig, listFor(payload, id));
        note("member_list.id");
        return listFor(payload, payload.id);
      }
      if (name === "guildMembersChunk" && payload.not_found?.length) {
        for (const userId of payload.not_found) {
          const key = `${payload.guild_id}:${userId}`;
          if (fetchedMembers.has(key)) continue;
          fetchedMembers.add(key);
          missingMembers.add(key);
        }
        payload.not_found = payload.not_found.filter(userId => !missingMembers.has(`${payload.guild_id}:${userId}`));
        note("members_chunk.not_found");
        fetchMissingMembers();
        return payload;
      }
      if (name === "guildRoleUpdate" && payload.role?.id === payload.guild_id && rememberColor(payload.guild_id, payload.role)) phoenix.dispatch(phoenix.gateway.guildRoleCreate, { guild_id: payload.guild_id, role: colorRoleOf(payload.role) });
      if (changed) return payload;
    },
  });
}

phoenix.discord.onResponse({ pathIncludes: "/profile" }, async res => {
  if (res.status !== 304) return;
  const fresh = await phoenix.discord.get(res.path);
  const body = fresh.ok ? fresh.json() : null;
  if (body) {
    fixDeep(body);
    return { status: 200, body };
  }
});

phoenix.discord.onResponse({ pathIncludes: "/" }, res => {
  if (res.status >= 300 || !FIXABLE.test(res.body)) return;
  const body = res.json();
  if (body && fixDeep(body)) return { body };
});
