export const EVENT_RAW: number;
export const MEDIA_EVENT_TRACK_ADDED: number;
export const MEDIA_EVENT_TRACK_REMOVED: number;
export const DELIVERY_RELIABLE: number;
export const DELIVERY_UNRELIABLE_ORDERED: number;
export const DELIVERY_UNRELIABLE: number;

export function storage_add(path: string, data: Uint8Array): Promise<string>;
export function storage_get(path: string): Promise<Uint8Array>;

export type DeliveryMethod =
    | typeof DELIVERY_RELIABLE
    | typeof DELIVERY_UNRELIABLE_ORDERED
    | typeof DELIVERY_UNRELIABLE;

export interface MediaEventPayload {
    fromId: string;
    trackId: string;
    kind: string;
    track: MediaStreamTrack;
    stream?: MediaStream;
}

export interface LocalTrackOptions {
    enabled?: boolean;
    publish?: boolean;
    stopPrevious?: boolean;
    prefix?: string;
}

export class MistNode {
    constructor(nodeId: string, signalingUrl?: string);

    init(): Promise<void>;
    onEvent(handler: (eventType: number, fromId: string, payload: unknown) => void): void;
    onRawMessage(handler: (fromId: string, payload: Uint8Array) => void): void;
    onMediaEvent(handler: (eventType: number, payload: MediaEventPayload) => void): void;
    onRemoteTrack(handler: (payload: MediaEventPayload) => void): void;

    joinRoom(roomId: string): void;
    leaveRoom(): void;
    updatePosition(x: number, y: number, z?: number): void;

    getNeighbors(): unknown[];
    getAllNodes(): unknown[];
    getConfig(): Record<string, unknown>;
    setConfig(config: string | Record<string, unknown>): boolean;
    getStats(): Record<string, unknown>;

    sendMessage(
        toId: string | null | undefined,
        payload: Uint8Array | ArrayBuffer | string | Record<string, unknown>,
        delivery?: DeliveryMethod,
    ): void;

    createLocalMedia(constraints?: MediaStreamConstraints): Promise<MediaStream>;
    createDisplayMedia(constraints?: DisplayMediaStreamOptions): Promise<MediaStream>;

    registerLocalTrack(trackId: string, track: MediaStreamTrack, options?: LocalTrackOptions): MediaStreamTrack;
    replaceLocalTrack(trackId: string, track: MediaStreamTrack, options?: LocalTrackOptions): MediaStreamTrack;
    getLocalTrack(trackId: string): MediaStreamTrack | null;
    publishLocalTrack(trackId: string): void;
    unpublishLocalTrack(trackId: string): void;
    removeLocalTrack(trackId: string): void;
    setLocalTrackEnabled(trackId: string, enabled: boolean): void;

    addLocalStream(
        stream: MediaStream,
        options?: LocalTrackOptions,
    ): Promise<Array<{ trackId: string; track: MediaStreamTrack }>>;

    attachMedia<T extends HTMLMediaElement>(element: T, trackOrStream: MediaStreamTrack | MediaStream): MediaStream;
}
