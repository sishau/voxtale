#!/usr/bin/env python3
# -*- coding: utf-8 -*-

import atexit
import hashlib
import json
import os
import re
import time
from pathlib import Path

from logger import logger


class text:
    def __init__(self, conf):
        self.conf = conf
        self.cur_chapter_index = 0
        self.cur_chapter_pos = 0
        self.book_conf = None
        self.chapter_list = []
        self.book_name = ""
        self.save_path = "./logs"
        self.encoding = self.conf.get("encoding", "utf-8")
        self._last_progress_write = 0.0
        self._progress_write_interval = 600.0
        atexit.register(self._save_on_exit)

    def _save_on_exit(self):
        if self.book_conf is not None:
            self.save_book_progress(self.cur_chapter_index, self.cur_chapter_pos, force=True)

    def _get_title(self, index):
        if 0 <= index < len(self.chapter_list):
            return self.chapter_list[index]
        return ""

    def _split_text(self, content, chap_index, position):
        result = [
            {"text": f"{self.book_name}...{self._get_title(chap_index)}",
             "chapterIndex": chap_index, "position": position}
        ]
        cur_pos = 0
        for line in content.strip().split("\n"):
            cur_pos += len(line)
            if cur_pos < position:
                continue
            clean = "".join(x for x in line if x.isprintable())
            if len(clean) == 0:
                continue
            result.append({"text": re.sub(r"\s", "", clean),
                           "chapterIndex": chap_index, "position": cur_pos})
        return result

    def cal_file_md5(self, file_path, chunk_size=8192):
        st = time.time()
        md5_hash = hashlib.md5()
        with open(file_path, "rb") as file:
            while chunk := file.read(chunk_size):
                md5_hash.update(chunk)
        logger.debug(f"计算文件md5耗时: {time.time() - st}")
        return md5_hash.hexdigest()

    def _get_book_config(self, conf_file):
        with open(conf_file, "r", encoding="utf-8") as f:
            data = json.load(f)
        if data["file_path"] != self.file_path:
            raise Exception(f"文件位置不一致：{data['file_path']} -> {self.file_path}")
        return data

    def _get_book_content(self, chapter_index=None):
        if chapter_index is None:
            chapter_index = self.cur_chapter_index
        chap_length, chap_pointer = self.book_conf["chapterInfo"][chapter_index]
        with open(self.file_path, "r", encoding=self.encoding) as f:
            f.seek(chap_pointer)
            return f.read(chap_length)

    def parse_volumes_and_chapters(self):
        volume_pattern = r"^第([一二三四五六七八九十\d]+)卷\s*(.*)"
        chapter_pattern = r"^第([一二三四五六七八九十百千\d]+)章\s*(.*)"
        current_volume = None
        chap_list = []
        chap_info = []
        with open(self.file_path, "r", encoding=self.encoding) as f:
            words = 0
            chap_pointer_last = -1
            chap_pointer = 0
            while True:
                cur_pointer = f.tell()
                line = f.readline()
                if not line:
                    break
                words += len(line)
                volume_match = re.search(volume_pattern, line)
                if volume_match:
                    current_volume = volume_match.group()
                    continue
                chapter_match = re.search(chapter_pattern, line)
                if chapter_match:
                    current_chapter = chapter_match.group()
                    chap_name = f"{current_volume} {current_chapter}" if current_volume else current_chapter
                    chap_list.append(chap_name)
                    chap_pointer = cur_pointer
                    if chap_pointer_last != -1:
                        chap_info.append((words, chap_pointer_last))
                    chap_pointer_last = chap_pointer
                    words = 0
            chap_info.append((words, chap_pointer))
        return chap_list, chap_info

    def initialize(self):
        if "path" not in self.conf:
            raise Exception("请设置待阅读的txt文件所在路径")
        self.file_path = os.path.expanduser(self.conf["path"])
        logger.info(f"文件位置：{self.file_path}")
        if not os.path.exists(self.file_path):
            raise Exception("路径错误，文件不存在")
        file_ = Path(self.file_path)
        self.book_name = file_.stem
        if file_.suffix != ".txt":
            raise Exception(f"此方式只支持txt文件, 而不是{file_.suffix}")

        md5 = self.cal_file_md5(self.file_path)
        self.read_config_file = f"{self.save_path}/text_{md5}.json"
        if not os.path.exists(self.read_config_file):
            chap_list, chap_info = self.parse_volumes_and_chapters()
            with open(self.read_config_file, "w", encoding="utf-8") as f:
                json.dump({
                    "file_path": self.file_path,
                    "durChapterIndex": 0,
                    "durChapterPos": 0,
                    "chapList": chap_list,
                    "chapterInfo": chap_info
                }, f, ensure_ascii=False)
        self.book_conf = self._get_book_config(self.read_config_file)
        self.cur_chapter_index = self.book_conf.get("durChapterIndex", 0)
        self.cur_chapter_pos = self.book_conf.get("durChapterPos", 0)
        self.chapter_list = self.book_conf["chapList"]
        logger.info(f"上次读取的位置：{self.cur_chapter_index}, {self.cur_chapter_pos}")

    def save_book_progress(self, chapter_index=None, chapter_pos=None, force=False):
        if chapter_index is not None:
            self.book_conf["durChapterIndex"] = chapter_index
            self.cur_chapter_index = chapter_index
        if chapter_pos is not None:
            self.book_conf["durChapterPos"] = chapter_pos
            self.cur_chapter_pos = chapter_pos
        now = time.time()
        if not force and now - self._last_progress_write < self._progress_write_interval:
            return
        self._write_progress()

    def _write_progress(self):
        with open(self.read_config_file, "w", encoding="utf-8") as f:
            json.dump(self.book_conf, f, ensure_ascii=False)
        self._last_progress_write = time.time()

    def flush_progress(self):
        if self.book_conf is not None:
            self._write_progress()

    def GenText(self, start_index=None, start_pos=None):
        index = self.cur_chapter_index if start_index is None else start_index
        pos = self.cur_chapter_pos if start_pos is None else start_pos
        while index < len(self.book_conf["chapterInfo"]):
            chapter_content = self._get_book_content(index)
            logger.info(f"Generating text from chapter {self._get_title(index)}")
            if not chapter_content:
                logger.error("Failed to get book content from server.")
            text_list = self._split_text(chapter_content, index, pos)
            for text in text_list:
                logger.debug(f"Yielding {len(text['text'])} characters of text.")
                yield text
            index += 1
            pos = 0


if __name__ == "__main__":
    import yaml
    with open("config/config.yaml", "r", encoding="utf-8") as f:
        conf = yaml.safe_load(f)["server"]
    svr = text(conf)
    svr.initialize()
    gen = svr.GenText()
    for _ in range(3):
        print(next(gen)["text"][:50])
