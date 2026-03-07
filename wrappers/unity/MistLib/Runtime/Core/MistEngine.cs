using System;
using System.Collections.Concurrent;
using System.Runtime.InteropServices;
using System.Text;
using UnityEngine;

namespace MistLib
{
    public class MistEngine : MonoBehaviour
    {
        public enum DeliveryMethod : uint
        {
            ReliableOrdered = 0,
            UnreliableOrdered = 1,
            Unreliable = 2
        }

        public static MistEngine I { get; private set; }
        public string SelfId { get; private set; }

        private static MistFFI.EventCallback _staticEventCallback;
        private static MistFFI.LogCallback _staticLogCallback;
        private static readonly ConcurrentQueue<(uint type, string from, byte[] data)> _eventQueue = new();

        [SerializeField] private string _roomId = "lobby";
        [SerializeField] private string _customNodeId = "";
        [SerializeField] private string _signalingUrl = "wss://rtc.tik-choco.com/signaling";

        public event Action<uint, string, byte[]> OnEvent;

        private void Awake()
        {
            if (I != null)
            {
                Destroy(gameObject);
                return;
            }
            I = this;
            DontDestroyOnLoad(gameObject);

            _staticEventCallback = OnEventFromRust;
            _staticLogCallback = OnLogFromRust;
            
            SelfId = string.IsNullOrEmpty(_customNodeId) ? Guid.NewGuid().ToString() : _customNodeId;
            Init(SelfId, _signalingUrl);
            
            MistFFI.register_event_callback(_staticEventCallback);
            MistFFI.register_log_callback(_staticLogCallback);
        }

        private void Start()
        {
            JoinRoom(_roomId);
            MistLogger.Log($"Engine initialized and joined room: {_roomId}");
        }

        private void OnDestroy()
        {
            if (I == this)
            {
                MistFFI.leave_room();
                I = null;
            }
        }

        private void Update()
        {
            while (_eventQueue.TryDequeue(out var ev))
            {
                OnHandleEvent(ev.type, ev.from, ev.data);
            }
        }

        protected virtual void OnHandleEvent(uint type, string from, byte[] data)
        {
            OnEvent?.Invoke(type, from, data);
        }

        public void Init(string nodeId, string signalingUrl)
        {
            var idBytes = Encoding.UTF8.GetBytes(nodeId);
            var urlBytes = Encoding.UTF8.GetBytes(signalingUrl);
            
            PinAndCall(idBytes, (idPtr, idLen) => {
                PinAndCall(urlBytes, (urlPtr, urlLen) => {
                    MistFFI.init(idPtr, idLen, urlPtr, urlLen);
                });
            });
        }

        public void JoinRoom(string roomId)
        {
            var bytes = Encoding.UTF8.GetBytes(roomId);
            PinAndCall(bytes, (ptr, len) => MistFFI.join_room(ptr, len));
        }

        public void UpdatePosition(Vector3 pos) => MistFFI.update_position(pos.x, pos.y, pos.z);

        public void SendMessage(string targetNodeId, byte[] data, DeliveryMethod method = DeliveryMethod.Unreliable)
        {
            var idBytes = string.IsNullOrEmpty(targetNodeId) ? Array.Empty<byte>() : Encoding.UTF8.GetBytes(targetNodeId);
            PinAndCall(idBytes, (idPtr, idLen) => {
                PinAndCall(data, (dataPtr, dataLen) => {
                    MistFFI.send_message(idPtr, idLen, dataPtr, dataLen, (uint)method);
                });
            });
        }

        public string GetStats()
        {
            var buffer = new byte[4096];
            uint written = 0;
            var handle = GCHandle.Alloc(buffer, GCHandleType.Pinned);
            try
            {
                written = MistFFI.get_stats(handle.AddrOfPinnedObject(), (UIntPtr)buffer.Length);
            }
            finally
            {
                handle.Free();
            }

            if (written > 0 && written <= buffer.Length)
            {
                return Encoding.UTF8.GetString(buffer, 0, (int)written);
            }
            return "{}";
        }

        private void PinAndCall(byte[] data, Action<IntPtr, UIntPtr> action)
        {
            if (data == null || data.Length == 0)
            {
                action(IntPtr.Zero, UIntPtr.Zero);
                return;
            }
            var handle = GCHandle.Alloc(data, GCHandleType.Pinned);
            try
            {
                action(handle.AddrOfPinnedObject(), (UIntPtr)data.Length);
            }
            finally
            {
                handle.Free();
            }
        }

        [AOT.MonoPInvokeCallback(typeof(MistFFI.EventCallback))]
        private static void OnEventFromRust(uint eventType, IntPtr from, UIntPtr fromLen, IntPtr data, UIntPtr dataLen)
        {
            try
            {
                var fromBuffer = new byte[(int)fromLen];
                Marshal.Copy(from, fromBuffer, 0, (int)fromLen);
                var fromId = Encoding.UTF8.GetString(fromBuffer);

                var dataBuffer = new byte[(int)dataLen];
                Marshal.Copy(data, dataBuffer, 0, (int)dataLen);
                
                _eventQueue.Enqueue((eventType, fromId, dataBuffer));
            }
            catch (Exception ex)
            {
                Debug.LogException(ex);
            }
        }

        [AOT.MonoPInvokeCallback(typeof(MistFFI.LogCallback))]
        private static void OnLogFromRust(uint level, IntPtr data, UIntPtr len)
        {
            try
            {
                var buffer = new byte[(int)len];
                Marshal.Copy(data, buffer, 0, (int)len);
                var message = Encoding.UTF8.GetString(buffer);
                MistLogger.Log(message, (MistLogger.LogLevel)level);
            }
            catch (Exception ex)
            {
                System.Console.WriteLine($"[MistLib Log Callback Error] {ex.Message}");
            }
        }
    }
}
