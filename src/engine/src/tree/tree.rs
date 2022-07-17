use super::{
    node::{DataNodeIter, IndexNodeIter, NodeId, NodeIndex, NodePair, PageView},
    page::{
        DataPageBuf, DataPageLayout, DataPageRef, DataRecord, IndexPageRef, MergeIterBuilder,
        PageBuf, PageKind, PageLayout, PagePtr, PageRef,
    },
    pagealloc::PageAlloc,
    pagestore::PageStore,
    pagetable::PageTable,
    Error, Ghost, Options, Result,
};

pub struct Tree {
    alloc: PageAlloc,
    table: PageTable,
    store: PageStore,
}

impl Tree {
    pub async fn open(opts: Options) -> Result<Self> {
        let alloc = PageAlloc::default();
        let table = PageTable::default();
        let store = PageStore::open(opts).await?;
        let tree = Self {
            alloc,
            table,
            store,
        };
        tree.recover().await?;
        Ok(tree)
    }

    async fn recover(&self) -> Result<()> {
        // TODO: recovers the page table from the page store.
        Ok(())
    }

    pub async fn get<'g>(&self, key: &[u8], ghost: &'g Ghost) -> Result<Option<&'g [u8]>> {
        loop {
            match self.try_get(key, ghost).await {
                Err(Error::Conflict) => continue,
                other => return other,
            }
        }
    }

    async fn try_get<'g>(&self, key: &[u8], ghost: &'g Ghost) -> Result<Option<&'g [u8]>> {
        let node = self.try_find_data_node(key, ghost).await?;
        self.search_data_node(&node, key, ghost).await
    }

    pub async fn put<'g>(
        &self,
        lsn: u64,
        key: &[u8],
        value: &[u8],
        ghost: &'g Ghost,
    ) -> Result<()> {
        let record = DataRecord::put(lsn, key, value);
        self.update(&record, ghost).await
    }

    pub async fn delete<'g>(&self, lsn: u64, key: &[u8], ghost: &'g Ghost) -> Result<()> {
        let record = DataRecord::delete(lsn, key);
        self.update(&record, ghost).await
    }

    async fn update<'g>(&self, record: &DataRecord<'g>, ghost: &'g Ghost) -> Result<()> {
        let mut layout = DataPageLayout::default();
        layout.add(&record);
        let mut page: DataPageBuf = self.alloc.alloc(&layout);
        page.add(&record);
        loop {
            match self.try_update(record.key, page.as_ptr(), ghost).await {
                Ok(_) => {
                    std::mem::forget(page);
                    return Ok(());
                }
                Err(Error::Conflict) => continue,
                Err(err) => {
                    self.alloc.dealloc(page.into());
                    return Err(err);
                }
            }
        }
    }

    async fn try_update<'g>(&self, key: &[u8], delta: PagePtr, ghost: &'g Ghost) -> Result<()> {
        let mut node = self.try_find_data_node(key, ghost).await?;
        loop {
            match self.update_node(node.id, node.view.as_ptr(), delta) {
                None => return Ok(()),
                Some(now) => {
                    let view = self.page_view(now, ghost);
                    if view.ver() != node.view.ver() {
                        return Err(Error::Conflict);
                    }
                    node.view = view;
                }
            }
        }
    }
}

impl Tree {
    fn page_ptr(&self, id: NodeId) -> PagePtr {
        self.table.get(id.into()).into()
    }

    fn page_view<'g>(&self, ptr: PagePtr, _: &'g Ghost) -> PageView<'g> {
        match ptr {
            PagePtr::Mem(addr) => PageView::Mem(addr.into()),
            PagePtr::Disk(addr) => {
                let addr = addr.into();
                let info = self.store.page_info(addr).unwrap();
                PageView::Disk(addr, info)
            }
        }
    }

    fn node_pair<'g>(&self, id: NodeId, ghost: &'g Ghost) -> NodePair<'g> {
        let ptr = self.page_ptr(id);
        let view = self.page_view(ptr, ghost);
        NodePair::new(id, view)
    }

    fn update_node<'g>(&self, id: NodeId, old: PagePtr, new: PagePtr) -> Option<PagePtr> {
        self.table
            .cas(id.into(), old.into(), new.into())
            .map(|now| now.into())
    }

    fn swapin_page<'g>(
        &self,
        id: NodeId,
        ptr: PagePtr,
        buf: PageBuf,
        ghost: &'g Ghost,
    ) -> Result<PageRef<'g>> {
        todo!()
    }

    fn swapout_page<'g>(&self, id: NodeId, ptr: PagePtr, ghost: &'g Ghost) -> Result<PageRef<'g>> {
        todo!()
    }

    async fn load_page_with_ptr<'g>(
        &self,
        id: NodeId,
        ptr: PagePtr,
        ghost: &'g Ghost,
    ) -> Result<PageRef<'g>> {
        match ptr {
            PagePtr::Mem(addr) => Ok(addr.into()),
            PagePtr::Disk(addr) => {
                let buf = self.store.load_page_with_addr(addr.into()).await?;
                self.swapin_page(id, ptr, buf.into(), ghost)
            }
        }
    }

    async fn load_page_with_view<'g>(
        &self,
        id: NodeId,
        view: &PageView<'g>,
        ghost: &'g Ghost,
    ) -> Result<PageRef<'g>> {
        match *view {
            PageView::Mem(page) => Ok(page),
            PageView::Disk(addr, ref info) => {
                let ptr = PagePtr::Disk(addr.into());
                let buf = self.store.load_page_with_handle(&info.handle).await?;
                let page = PageBuf::from(buf);
                if page.ver() != view.ver() {
                    return Err(Error::Conflict);
                }
                self.swapin_page(id, ptr, page, ghost)
            }
        }
    }

    async fn try_find_data_node<'g>(&self, key: &[u8], ghost: &'g Ghost) -> Result<NodePair<'g>> {
        let mut cursor = NodeIndex::root();
        let mut parent = None;
        loop {
            let node = self.node_pair(cursor.id, ghost);
            if node.view.ver() != cursor.ver {
                self.try_help_pending_smo(&node, parent.as_ref(), ghost)?;
                return Err(Error::Conflict);
            }
            if node.is_data() {
                return Ok(node);
            }
            cursor = self.search_index_node(&node, key, ghost).await?;
            parent = Some(node);
        }
    }

    fn try_help_pending_smo<'g>(
        &self,
        node: &NodePair<'g>,
        parent: Option<&NodePair<'g>>,
        ghost: &'g Ghost,
    ) -> Result<()> {
        todo!()
    }

    async fn iter_data_node<'g>(
        &self,
        node: &NodePair<'g>,
        ghost: &'g Ghost,
    ) -> Result<DataNodeIter<'g>> {
        let mut page = self.load_page_with_view(node.id, &node.view, ghost).await?;
        let mut merger = MergeIterBuilder::default();
        loop {
            match page.kind() {
                PageKind::Data => {
                    let page = DataPageRef::from(page);
                    merger.add(page.iter());
                }
                _ => unreachable!(),
            }
            if let Some(next) = page.next() {
                page = self.load_page_with_ptr(node.id, next, ghost).await?;
            } else {
                return Ok(merger.build());
            }
        }
    }

    async fn search_data_node<'g>(
        &self,
        node: &NodePair<'g>,
        key: &[u8],
        ghost: &'g Ghost,
    ) -> Result<Option<&'g [u8]>> {
        let mut page = self.load_page_with_view(node.id, &node.view, ghost).await?;
        loop {
            match page.kind() {
                PageKind::Data => {
                    let page = DataPageRef::from(page);
                    if let Some(record) = page.get(key) {
                        todo!()
                    }
                }
                _ => unreachable!(),
            }
            if let Some(next) = page.next() {
                page = self.load_page_with_ptr(node.id, next, ghost).await?;
            } else {
                return Ok(None);
            }
        }
    }

    async fn try_split_data_node<'g>(
        &self,
        node: &NodePair<'g>,
        key: &[u8],
        ghost: &'g Ghost,
    ) -> Result<()> {
        todo!()
    }

    async fn try_consolidate_data_node<'g>(
        &self,
        node: &NodePair<'g>,
        ghost: &'g Ghost,
    ) -> Result<()> {
        let iter = self.iter_data_node(node, ghost).await?;
        let mut layout = DataPageLayout::default();
        for record in iter {
            layout.add(&record);
        }
        let mut page: DataPageBuf = self.alloc.alloc(&layout);
        if self
            .update_node(node.id, node.view.as_ptr(), page.as_ptr())
            .is_some()
        {
            return Err(Error::Conflict);
        }
        /*
        if page.size() >= self.opts.data_node_size {
            if let Some(split_key) = page.split_key() {
                self.try_split_data_node(node, split_key, ghost).await?;
            }
        }
        */
        todo!()
    }

    async fn iter_index_node<'g>(
        &self,
        node: &NodePair<'g>,
        ghost: &'g Ghost,
    ) -> Result<IndexNodeIter<'g>> {
        let mut page = self.load_page_with_view(node.id, &node.view, ghost).await?;
        let mut merger = MergeIterBuilder::default();
        loop {
            match page.kind() {
                PageKind::Index => {
                    let page = IndexPageRef::from(page);
                    merger.add(page.iter());
                }
                _ => unreachable!(),
            }
            if let Some(next) = page.next() {
                page = self.load_page_with_ptr(node.id, next, ghost).await?;
            } else {
                return Ok(merger.build());
            }
        }
    }

    async fn search_index_node<'g>(
        &self,
        node: &NodePair<'g>,
        key: &[u8],
        ghost: &'g Ghost,
    ) -> Result<NodeIndex> {
        todo!()
    }

    async fn try_consolidate_index_node<'g>(
        &self,
        node: &NodePair<'g>,
        ghost: &'g Ghost,
    ) -> Result<()> {
        todo!()
    }
}