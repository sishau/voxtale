#! /usr/bin/env python
# -*- coding: utf-8 -*-

import logging
from logging.config import dictConfig
import os
import yaml
from datetime import datetime

project_folder = os.path.dirname(os.path.abspath(__file__))
logger_folder = os.path.join(project_folder, 'logs')
if not os.path.exists(logger_folder):
    os.makedirs(logger_folder)
logger_file = os.path.join(logger_folder, "voxtale-{}.log".format(datetime.now().strftime("%Y-%m-%d")))


with open(os.path.join(project_folder, 'config', 'logger.yaml'), 'r', encoding='utf-8') as f:
    logger_config = yaml.safe_load(f)

logger_config['handlers']['file']['filename'] = logger_file
dictConfig(logger_config)

logger = logging.getLogger("app")
