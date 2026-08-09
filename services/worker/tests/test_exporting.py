import json
import sys
import tempfile
import unittest
import zipfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from remin_worker.exporting import export_table


class ExportingTests(unittest.TestCase):
    def test_writes_all_supported_formats_without_overwriting(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for export_format, member in (("csv", None), ("json", None), ("xlsx", "xl/workbook.xml"), ("docx", "word/document.xml")):
                target = root / f"拾忆结果.{export_format}"
                result, error = export_table({"target_path": str(target), "format": export_format, "headers": ["文件名", "金额"], "rows": [["合同.docx", "1200"]]})
                self.assertIsNone(error)
                self.assertEqual(result["row_count"], 1)
                self.assertTrue(target.is_file())
                if member:
                    with zipfile.ZipFile(target) as archive:
                        self.assertIn(member, archive.namelist())
                second, second_error = export_table({"target_path": str(target), "format": export_format, "headers": ["文件名"], "rows": [["覆盖"]]})
                self.assertIsNone(second)
                self.assertEqual(second_error.code, "TARGET_EXISTS")
            self.assertEqual(json.loads((root / "拾忆结果.json").read_text(encoding="utf-8"))[0]["文件名"], "合同.docx")


if __name__ == "__main__":
    unittest.main()
