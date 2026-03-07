using System;
using UnityEngine;

namespace MistNet
{
    [Serializable]
    public struct NodeId : IEquatable<NodeId>
    {
        public string Id;
        public NodeId(string id) => Id = id;
        public override string ToString() => Id;
        public static implicit operator string(NodeId nodeId) => nodeId.Id;
        public static implicit operator NodeId(string id) => new NodeId(id);
        public bool Equals(NodeId other) => Id == other.Id;
        public override bool Equals(object obj) => obj is NodeId other && Equals(other);
        public override int GetHashCode() => Id != null ? Id.GetHashCode() : 0;
        public static bool operator ==(NodeId left, NodeId right) => left.Equals(right);
        public static bool operator !=(NodeId left, NodeId right) => !left.Equals(right);
    }

    [Serializable]
    public struct ObjectId : IEquatable<ObjectId>
    {
        public string Id;
        public ObjectId(string id) => Id = id;
        public override string ToString() => Id;
        public static implicit operator string(ObjectId objId) => objId.Id;
        public static implicit operator ObjectId(string id) => new ObjectId(id);
        public bool Equals(ObjectId other) => Id == other.Id;
        public override bool Equals(object obj) => obj is ObjectId other && Equals(other);
        public override int GetHashCode() => Id != null ? Id.GetHashCode() : 0;
        public static bool operator ==(ObjectId left, ObjectId right) => left.Equals(right);
        public static bool operator !=(ObjectId left, ObjectId right) => !left.Equals(right);
    }
}
