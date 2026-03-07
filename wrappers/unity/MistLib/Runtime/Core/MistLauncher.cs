using UnityEngine;

namespace MistLib
{
    public class MistLauncher : MonoBehaviour
    {
        [SerializeField] private GameObject _playerPrefab;

        private void Start()
        {
            SpawnLocalPlayer();
        }

        private void SpawnLocalPlayer()
        {
            var obj = Instantiate(_playerPrefab, Vector3.zero, Quaternion.identity);
            var sync = obj.GetComponent<MistSyncObject>();
            
            var selfId = MistEngine.I.SelfId;
            sync.Init(selfId, selfId);
            
            MistSyncManager.I.RegisterObject(sync);
            MistLogger.Log($"Spawned local player: {selfId}");
        }
    }
}
