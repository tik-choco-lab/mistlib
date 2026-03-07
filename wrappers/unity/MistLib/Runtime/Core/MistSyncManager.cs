using System.Collections.Generic;
using UnityEngine;

namespace MistLib
{
    public class MistSyncManager : MonoBehaviour
    {
        public static MistSyncManager I { get; private set; }

        private readonly Dictionary<string, MistSyncObject> _objects = new();

        private void Awake()
        {
            if (I != null) { Destroy(gameObject); return; }
            I = this;
        }

        private void Start()
        {
            MistEngine.I.OnEvent += OnMistEvent;
        }

        private void OnMistEvent(uint type, string from, byte[] data)
        {
            if (type == (uint)MistLibMessageType.Location)
            {
                BincodeReader.DeserializeLocation(data, out var objId, out _, out _, out _, out _, out _);
                if (_objects.TryGetValue(objId, out var obj))
                {
                    if (obj.TryGetComponent<MistTransform>(out var trans))
                    {
                        trans.OnReceiveLocation(data);
                    }
                }
            }
        }

        public void RegisterObject(MistSyncObject obj)
        {
            _objects[obj.Id] = obj;
        }

        public void UnregisterObject(MistSyncObject obj)
        {
            _objects.Remove(obj.Id);
        }
    }
}
