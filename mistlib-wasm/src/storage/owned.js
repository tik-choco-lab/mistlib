// Transfer the backing buffer synchronously, before the API returns its Promise.
// structuredClone preserves a subarray's offset/length while detaching ALL aliases.
// Unsupported, shared, detached and non-transferable buffers throw; never copy as
// a fallback, since that would restore the unbounded snapshot task.
export function takeStorageInput(data) {
    return structuredClone(data, { transfer: [data.buffer] });
}
