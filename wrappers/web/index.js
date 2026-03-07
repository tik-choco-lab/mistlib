import init, {
    init as mist_init,
    update_position as mist_update_position,
    get_neighbors as mist_get_neighbors,
    get_all_nodes as mist_get_all_nodes,
    join_room as mist_join_room,
    get_config as mist_get_config,
    set_config as mist_set_config,
    get_stats as mist_get_stats,
    send_message as mist_send_message,
    leave_room as mist_leave_room,
    register_event_callback as mist_register_event_callback,
    register_media_event_callback as mist_register_media_event_callback,
    register_local_track as mist_register_local_track,
    get_local_track as mist_get_local_track,
    publish_local_track as mist_publish_local_track,
    unpublish_local_track as mist_unpublish_local_track,
    remove_local_track as mist_remove_local_track,
    set_local_track_enabled as mist_set_local_track_enabled,
    storage_add as mist_storage_add,
    storage_get as mist_storage_get,
} from '../../mistlib-wasm/pkg/mistlib_wasm.js';

export const EVENT_RAW = 0;
export const MEDIA_EVENT_TRACK_ADDED = 100;
export const MEDIA_EVENT_TRACK_REMOVED = 101;
export const DELIVERY_RELIABLE = 0;
export const DELIVERY_UNRELIABLE_ORDERED = 1;
export const DELIVERY_UNRELIABLE = 2;
export const storage_add = mist_storage_add;
export const storage_get = mist_storage_get;

export class MistNode {
    constructor(nodeId, signalingUrl = "wss://rtc.tik-choco.com/signaling") {
        this.nodeId = nodeId;
        this.signalingUrl = signalingUrl;
        this.initialized = false;
        this._onEvent = null;
        this._onMediaEvent = null;
    }

    async init() {
        if (this.initialized) return;
        await init();
        mist_init(this.nodeId, this.signalingUrl);
        mist_register_event_callback((eventType, fromId, payload) => {
            if (this._onEvent) {
                this._onEvent(eventType, fromId, payload);
            }
        });
        mist_register_media_event_callback((eventType, fromId, trackId, kind, track, stream) => {
            if (this._onMediaEvent) {
                this._onMediaEvent(eventType, {
                    fromId,
                    trackId,
                    kind,
                    track,
                    stream: stream ?? undefined,
                });
            }
        });
        this.initialized = true;
    }

    onEvent(handler) {
        this._onEvent = handler;
    }

    onRawMessage(handler) {
        this._onEvent = (eventType, fromId, payload) => {
            if (eventType !== EVENT_RAW) return;
            const bytes = payload instanceof Uint8Array ? payload : new Uint8Array(payload);
            handler(fromId, bytes);
        };
    }

    onMediaEvent(handler) {
        this._onMediaEvent = handler;
    }

    onRemoteTrack(handler) {
        this._onMediaEvent = (eventType, payload) => {
            if (eventType !== MEDIA_EVENT_TRACK_ADDED) return;
            handler(payload);
        };
    }

    joinRoom(roomId) {
        mist_join_room(roomId);
    }

    updatePosition(x, y, z = 0) {
        mist_update_position(x, y, z);
    }

    getNeighbors() {
        const neighborsJson = mist_get_neighbors();
        try {
            return JSON.parse(neighborsJson);
        } catch (e) {
            console.error("Failed to parse neighbors JSON:", e);
            return [];
        }
    }

    getAllNodes() {
        const allNodesJson = mist_get_all_nodes();
        try {
            return JSON.parse(allNodesJson);
        } catch (e) {
            console.error("Failed to parse all nodes JSON:", e);
            return [];
        }
    }

    getConfig() {
        const configJson = mist_get_config();
        try {
            return JSON.parse(configJson);
        } catch (e) {
            console.error("Failed to parse config JSON:", e);
            return {};
        }
    }

    setConfig(config) {
        const configJson = typeof config === 'string' ? config : JSON.stringify(config);
        return Boolean(mist_set_config(configJson));
    }

    getStats() {
        const statsJson = mist_get_stats();
        try {
            return JSON.parse(statsJson);
        } catch (e) {
            console.error("Failed to parse stats JSON:", e);
            return {};
        }
    }

    sendMessage(toId, payload, delivery = DELIVERY_UNRELIABLE) {
        const to = toId || "";
        let data;
        if (payload instanceof Uint8Array) {
            data = payload;
        } else if (payload instanceof ArrayBuffer) {
            data = new Uint8Array(payload);
        } else if (typeof payload === 'string') {
            data = new TextEncoder().encode(payload);
        } else {
            data = new TextEncoder().encode(JSON.stringify(payload));
        }
        mist_send_message(to, data, delivery);
    }

    async createLocalMedia(constraints = { audio: true, video: true }) {
        return navigator.mediaDevices.getUserMedia(constraints);
    }

    async createDisplayMedia(constraints = { video: true, audio: false }) {
        return navigator.mediaDevices.getDisplayMedia(constraints);
    }

    registerLocalTrack(trackId, track, options = {}) {
        mist_register_local_track(trackId, track);
        if (typeof options.enabled === 'boolean') {
            mist_set_local_track_enabled(trackId, options.enabled);
        }
        if (options.publish !== false) {
            mist_publish_local_track(trackId);
        }
        return track;
    }

    replaceLocalTrack(trackId, track, options = {}) {
        const previous = this.getLocalTrack(trackId);
        mist_register_local_track(trackId, track);
        if (typeof options.enabled === 'boolean') {
            mist_set_local_track_enabled(trackId, options.enabled);
        } else if (previous && previous.enabled !== track.enabled) {
            mist_set_local_track_enabled(trackId, previous.enabled);
        }
        if (options.publish !== false) {
            mist_publish_local_track(trackId);
        }
        if (options.stopPrevious !== false && previous && previous.id !== track.id) {
            previous.stop();
        }
        return track;
    }

    getLocalTrack(trackId) {
        return mist_get_local_track(trackId) ?? null;
    }

    publishLocalTrack(trackId) {
        mist_publish_local_track(trackId);
    }

    unpublishLocalTrack(trackId) {
        mist_unpublish_local_track(trackId);
    }

    removeLocalTrack(trackId) {
        mist_remove_local_track(trackId);
    }

    setLocalTrackEnabled(trackId, enabled) {
        mist_set_local_track_enabled(trackId, enabled);
    }

    async addLocalStream(stream, options = {}) {
        const entries = [];
        for (const track of stream.getTracks()) {
            const trackId = options.prefix ? `${options.prefix}:${track.id}` : track.id;
            this.registerLocalTrack(trackId, track, options);
            entries.push({ trackId, track });
        }
        return entries;
    }

    attachMedia(element, trackOrStream) {
        const stream = trackOrStream instanceof MediaStream
            ? trackOrStream
            : new MediaStream([trackOrStream]);
        element.srcObject = stream;
        return stream;
    }

    leaveRoom() {
        mist_leave_room();
        this.initialized = false;
    }
}
