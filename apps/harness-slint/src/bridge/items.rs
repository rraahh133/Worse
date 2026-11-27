use super::{
    item_id, markdown, one_line, thumbs, tool_status, turn_status, ChatItem, FileReq, FileRow,
    Item, ItemId, ListDir, Model, ModelRc, Notice, Rc, UiState, VecModel,
};

impl UiState {
    pub(super) fn chat_item(&mut self, id: ItemId, item: &Item) -> ChatItem {
        let mut row = ChatItem {
            id: item_id(id),
            ..ChatItem::default()
        };
        match item {
            Item::User {
                text,
                background: true,
                ..
            } => {
                // Autonomous turn (§9.17): one muted line with a refresh icon, not a user bubble.
                row.kind = 6;
                row.text = text.as_str().into();
            }
            Item::User {
                text,
                background: false,
                images,
            } => {
                row.kind = 0;
                row.text = match text.find("\n\n<harness-resume ") {
                    Some(at) => {
                        format!("{}\n\n(Resumed after an interruption)", &text[..at]).into()
                    }
                    None => text.as_str().into(),
                };
                (row.images, row.images_more) = self.row_images(id, images);
            }
            Item::Assistant {
                agent,
                text,
                status,
            } => {
                row.kind = 1;
                row.agent = self.agent_name(agent).into();
                row.text = text.as_str().into();
                row.status = turn_status(*status);
                let model = Rc::new(VecModel::from(markdown::render(text)));
                row.blocks = ModelRc::from(model.clone());
                self.blocks.insert(id, model);
                self.texts.insert(id, text.clone());
                if !matches!(status, crate::core::TurnStatus::Streaming) {
                    let media = super::media_refs::find(text, &self.workspace);
                    (row.images, row.images_more) = self.row_images(id, &media);
                }
            }
            Item::Tool {
                agent,
                title,
                status,
            } => {
                row.kind = 2;
                row.agent = self.agent_name(agent).into();
                row.title = one_line(title).into();
                row.status = tool_status(*status);
                let image: Vec<String> = thumbs::tool_image(title, &self.workspace)
                    .into_iter()
                    .collect();
                (row.images, row.images_more) = self.row_images(id, &image);
                if let Some((slug, wtitle, kind, details)) = self.work_detect_tool(title) {
                    row.work_slug = slug.into();
                    row.work_title = wtitle.into();
                    row.work_kind = kind.into();
                    row.work_details = details.into();
                }
            }
            Item::Notice {
                notice: Notice::Failover { from, to, reset },
            } => {
                row.kind = 3;
                row.from = self.agent_name(from).into();
                row.to = self.agent_name(to).into();
                row.reset = reset.clone().unwrap_or_default().into();
            }
            Item::Notice {
                notice: Notice::Paused { reset },
            } => {
                row.kind = 4;
                row.reset = reset.clone().unwrap_or_default().into();
            }
            Item::Notice {
                notice: Notice::Earlier { count },
            } => {
                row.kind = 7;
                row.text = count.to_string().into();
            }
        }
        row
    }

    pub(super) fn push_item(&mut self, id: ItemId, item: &Item) {
        if matches!(
            item,
            Item::User {
                background: false,
                ..
            }
        ) {
            crate::live::new_turn(&self.window);
            crate::machine_view::new_turn();
        }
        self.note_item(id, item);
        let row = self.chat_item(id, item);
        let index = self.chat.row_count();
        self.rows.insert(id, index);
        self.chat.push(row);
        self.mark_continued(index);
        self.regroup(index);
        self.sync_streaming(index);
    }

    pub(super) fn update_row(&self, id: ItemId, f: impl FnOnce(&mut ChatItem)) {
        let Some(&index) = self.rows.get(&id) else {
            return;
        };
        if let Some(mut row) = self.chat.row_data(index) {
            let status = row.status;
            f(&mut row);
            let tool = row.kind == 2;
            let answer_status = row.kind == 1 && row.status != status;
            self.chat.set_row_data(index, row);
            if tool {
                self.regroup(index);
            }
            if answer_status {
                self.sync_streaming(index);
            }
        }
    }

    pub(super) fn append_text(&mut self, id: ItemId, delta: &str) {
        let Some(text) = self.texts.get_mut(&id) else {
            return;
        };
        text.push_str(delta);
        let full = text.clone();
        if let Some(model) = self.blocks.get(&id) {
            // 只更新变化的块：流式时通常只有最后一块变化，Slint 只重绘该块。
            let next = markdown::render(&full);
            for (index, block) in next.iter().enumerate() {
                match model.row_data(index) {
                    Some(existing) if existing == *block => {}
                    Some(_) => model.set_row_data(index, block.clone()),
                    None => model.push(block.clone()),
                }
            }
            while model.row_count() > next.len() {
                model.remove(model.row_count() - 1);
            }
        }
        self.update_row(id, |row| row.text = full.as_str().into());
    }

    pub(super) fn toggle_file(&mut self, index: usize) {
        let Some(row) = self.files.row_data(index) else {
            return;
        };
        if !row.is_dir {
            return;
        }
        if row.expanded {
            let mut end = index + 1;
            while end < self.files.row_count()
                && self
                    .files
                    .row_data(end)
                    .is_some_and(|r| r.depth > row.depth)
            {
                end += 1;
            }
            for remove in (index + 1..end).rev() {
                self.files.remove(remove);
                self.file_paths.remove(remove);
            }
            self.files.set_row_data(
                index,
                FileRow {
                    expanded: false,
                    ..row
                },
            );
        } else if let Some(path) = self.file_paths.get(index) {
            let _ = self.files_tx.send(FileReq::List(ListDir {
                dir: path.clone(),
                depth: row.depth + 1,
            }));
        }
    }
}
