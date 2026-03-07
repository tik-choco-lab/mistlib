# MistLib Python Wrapper

MistLibのctypesラッパーです。Rustでビルドされたダイナミックライブラリ (`.dll` / `.so` / `.dylib`) をPythonから呼び出せるようにします。

## 📁 構成
- `mist/`: ラッパーの本体パッケージ
  - `core.py`: C FFI 定義と MistNode クラス
  - `transform.py`: 位置同期用ユーティリティ

## 🚀 使い方
このディレクトリを `PYTHONPATH` に追加することで、`import mist` が可能になります。

```python
import sys
import os
# アプリケーションから直接読み込む場合の例
sys.path.append(os.path.abspath("path/to/mistlib/wrappers/python"))
from mist import MistNode
```

動作例については、ルートディレクトリの [examples/python](../../examples/python) を参照してください。
負荷試験などのテストについては [tests/python](../../tests/python) を参照してください。
