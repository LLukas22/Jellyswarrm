import importlib.util
import os
from pathlib import Path
import unittest
from unittest.mock import Mock, patch


class LibrarySeedingTests(unittest.TestCase):
    def load_initializer(self, count="25"):
        spec = importlib.util.spec_from_file_location(
            "init_jellyfin", Path(__file__).with_name("init-jellyfin.py")
        )
        module = importlib.util.module_from_spec(spec)
        with patch.dict(
            os.environ,
            {
                "LIBRARY_COUNT": count,
                "SERVER_NAME": "Movies 1",
                "COLLECTION_NAME": "Movies",
                "COLLECTION_PATH": "/media/movies",
                "COLLECTION_TYPE": "movies",
            },
        ):
            spec.loader.exec_module(module)
        return module

    def test_creates_original_and_24_distinct_empty_libraries(self):
        module = self.load_initializer()
        client = Mock()
        client.jellyfin._get.return_value = []
        module.create_library(client)
        client.jellyfin._get.assert_called_once_with("Library/VirtualFolders")
        calls = client.jellyfin.add_media_library.call_args_list
        self.assertEqual(len(calls), 25)
        self.assertEqual(
            calls[0].kwargs,
            {"name": "Movies", "collectionType": "movies", "paths": ["/media/movies"]},
        )
        self.assertEqual(
            [call.kwargs["name"] for call in calls[1:]],
            [f"Movies 1 Pagination {index:02d}" for index in range(1, 25)],
        )
        self.assertTrue(all(call.kwargs["paths"] == [] for call in calls[1:]))
        client.jellyfin.refresh_library.assert_called_once_with()

    def test_rerun_preserves_existing_libraries(self):
        module = self.load_initializer()
        client = Mock()
        client.jellyfin._get.return_value = [
            {"Name": "Movies"},
            *[{"Name": f"Movies 1 Pagination {index:02d}"} for index in range(1, 25)],
            {"Name": "Manually created library"},
        ]
        module.create_library(client)
        client.jellyfin.add_media_library.assert_not_called()

    def test_single_library_mode(self):
        module = self.load_initializer("1")
        client = Mock()
        client.jellyfin._get.return_value = [{"Name": "Movies"}]
        module.create_library(client)
        client.jellyfin.add_media_library.assert_not_called()

    def test_invalid_count(self):
        for count in ("0", "-1", "invalid"):
            with self.subTest(count=count), self.assertRaises(ValueError):
                self.load_initializer(count)


if __name__ == "__main__":
    unittest.main()
