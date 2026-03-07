using UnityEngine;

namespace MistLib
{
    [RequireComponent(typeof(MistSyncObject))]
    public class MistTransform : MonoBehaviour
    {
        [SerializeField] private float _syncInterval = 0.1f;
        [SerializeField] private float _smoothTime = 0.12f;

        private MistSyncObject _syncObject;
        private float _lastSyncTime;
        private Vector3 _previousPosition;
        private ushort _sequence;

        private Vector3 _targetPosition;
        private Vector3 _targetRotation;
        private Vector3 _receivedVelocity;
        private Vector3 _currentVelocity;
        private ushort _lastSequence;

        private void Awake()
        {
            _syncObject = GetComponent<MistSyncObject>();
            _targetPosition = transform.position;
            _targetRotation = transform.eulerAngles;
        }

        private void Update()
        {
            if (_syncObject.IsOwner)
            {
                UpdateOwner();
            }
            else
            {
                UpdateRemote();
            }
        }

        private void UpdateOwner()
        {
            if (Time.time - _lastSyncTime < _syncInterval) return;
            
            var currentPos = transform.position;
            if (Vector3.Distance(currentPos, _previousPosition) < 0.001f) return;

            var velocity = (currentPos - _previousPosition) / (Time.time - _lastSyncTime);
            _previousPosition = currentPos;
            _lastSyncTime = Time.time;

            var data = BincodeWriter.SerializeLocation(
                _syncObject.Id,
                currentPos,
                transform.eulerAngles,
                velocity,
                _syncInterval,
                _sequence++
            );

            MistEngine.I.SendMessage("", data, MistEngine.DeliveryMethod.Unreliable);
        }

        private void UpdateRemote()
        {
            var extrapolationFactor = 0.8f;
            _targetPosition += _receivedVelocity * Time.deltaTime * extrapolationFactor;

            transform.position = Vector3.SmoothDamp(
                transform.position,
                _targetPosition,
                ref _currentVelocity,
                _smoothTime
            );

            transform.rotation = Quaternion.Slerp(
                transform.rotation,
                Quaternion.Euler(_targetRotation),
                Time.deltaTime * 10f
            );
        }

        public void OnReceiveLocation(byte[] data)
        {
            BincodeReader.DeserializeLocation(
                data,
                out _,
                out var pos,
                out var rot,
                out var vel,
                out var interval,
                out var seq
            );

            if (seq <= _lastSequence && _lastSequence != 65535) return;
            _lastSequence = seq;

            _targetPosition = pos;
            _targetRotation = rot;
            _receivedVelocity = vel;
        }
    }
}
