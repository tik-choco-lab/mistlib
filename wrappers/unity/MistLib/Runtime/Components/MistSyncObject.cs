using UnityEngine;

namespace MistLib
{
    public class MistSyncObject : MonoBehaviour
    {
        [field:SerializeField] public ObjectId Id { get; private set; }
        [field:SerializeField] public NodeId OwnerId { get; private set; }
        
        public bool IsOwner => OwnerId == MistEngine.I.SelfId;

        public void Init(ObjectId id, NodeId ownerId)
        {
            Id = id;
            OwnerId = ownerId;
        }

        public void SetOwner(NodeId newOwnerId)
        {
            OwnerId = newOwnerId;
        }
    }
}
