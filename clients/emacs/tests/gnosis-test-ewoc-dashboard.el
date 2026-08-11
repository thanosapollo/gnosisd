;;; gnosis-test-ewoc-dashboard.el --- EWOC dashboard regressions -*- lexical-binding: t; -*-

;;; Code:

(require 'cl-lib)
(require 'ert)
(require 'ewoc)
(require 'gnosis)
(require 'gnosis-dashboard)

(defvar gnosis-themata--ewoc)
(defvar gnosis-themata--loading-p)
(defvar gnosis-themata--max-reviews)

(declare-function gnosis-themata--current-id "gnosis-dashboard")
(declare-function gnosis-themata--pretty-print "gnosis-dashboard")
(declare-function gnosis-themata--pretty-print-default "gnosis-dashboard")

(defun gnosis-test-ewoc-dashboard--record (index)
  "Return valid compact thema record for INDEX."
  `(:thema_id ,(format "thema-%06d" index)
    :thema_revision 1 :state_revision 1 :review_count 0 :kind "basic"
    :keimenon ,(format "Question %d" index)
    :hypothesis [,gnosis-core--json-null] :answer ["Answer"] :tags []
    :archived ,gnosis-core--json-false
    :suspended ,gnosis-core--json-false))

(defun gnosis-test-ewoc-dashboard--page (start count &optional next generation)
  "Return catalog page from START containing COUNT records.
NEXT defaults to the JSON null terminal marker.  GENERATION defaults to g1."
  (let ((items (make-vector count nil)))
    (dotimes (offset count)
      (aset items offset (gnosis-test-ewoc-dashboard--record (+ start offset))))
    `(:generation ,(or generation "g1") :items ,items
      :next ,(or next gnosis-core--json-null))))

(defun gnosis-test-ewoc-dashboard--pages (total)
  "Return bounded catalog pages containing TOTAL records."
  (cl-loop for start from 0 below total by 100
           for count = (min 100 (- total start))
           for next = (if (< (+ start count) total)
                          (format "cursor-%06d" (+ start count))
                        gnosis-core--json-null)
           collect (gnosis-test-ewoc-dashboard--page start count next)))

(defun gnosis-test-ewoc-dashboard--ids ()
  "Return thema identities in current EWOC order."
  (mapcar (lambda (record) (plist-get record :thema_id))
          (ewoc-collect gnosis-themata--ewoc #'identity)))

(defun gnosis-test-ewoc-dashboard--window-start-id (window)
  "Return opaque thema identity at WINDOW start."
  (with-current-buffer (window-buffer window)
    (gnosis-themata--id-at-position (window-start window))))

(defun gnosis-test-ewoc-dashboard--assert-complete-records (total)
  "Assert current EWOC owns TOTAL unique records."
  (let ((ids (gnosis-test-ewoc-dashboard--ids)))
    (should (= total (length ids)))
    (should (= total (length (delete-dups (copy-sequence ids)))))))

(defun gnosis-test-ewoc-dashboard--projection ()
  "Return compact valid dashboard projection."
  '(:generated_at_us 1 :review_day 1
    :current_streak_days 1
    :today (:review_count 1 :new_count 1 :success_count 1
            :failure_count 0 :override_count 0)
    :window (:from_day 1 :through_day 1 :days 30 :active_days 1
             :totals (:review_count 1 :new_count 1 :success_count 1
                      :failure_count 0 :override_count 0)
             :average_per_calendar_day 0.03 :average_per_active_day 1.0
             :success_rate 1.0)
    :queue (:new_count 1 :due_count 1 :overdue_count 0 :suspended_count 0)
    :content (:active_themata 1 :archived_themata 0
              :active_documents 1 :active_nodes 1)
    :active_session gnosis-core--json-null))

(defmacro gnosis-test-ewoc-dashboard--drain (jobs)
  "Run queued timer JOBS and jobs they schedule."
  (declare (indent 0) (debug t))
  (let ((variable (if (eq (car-safe jobs) 'quote) (cadr jobs) jobs)))
    `(while ,variable
       (let ((job (pop ,variable)))
         (apply (car job) (cdr job))))))

(defmacro gnosis-test-ewoc-dashboard--with-client (catalog-function &rest body)
  "Evaluate BODY with CATALOG-FUNCTION and deterministic client boundaries."
  (declare (indent 1) (debug (form body)))
  `(let (jobs)
     (when-let* ((buffer (get-buffer gnosis-dashboard-buffer-name)))
       (kill-buffer buffer))
     (unwind-protect
         (cl-letf (((symbol-function 'gnosis-core-dashboard)
                    #'gnosis-test-ewoc-dashboard--projection)
                   ((symbol-function 'gnosis-core-thema-catalog)
                    ,catalog-function)
                   ((symbol-function 'gnosis-core--token)
                    (lambda () (make-string 40 ?a)))
                   ((symbol-function 'gnosis-core--validated-base-url)
                    (lambda () "https://test.example"))
                   ((symbol-function 'gnosis-core--authenticated-authority)
                    (lambda (&optional token)
                      (vector "https://test.example"
                              (secure-hash 'sha256
                                           (or token (make-string 40 ?a))))))
                   ((symbol-function 'run-at-time)
                    (lambda (_time _repeat function &rest arguments)
                      (setq jobs (append jobs (list (cons function arguments))))
                      'timer))
                   ((symbol-function 'keymap-popup) #'ignore)
                   ((symbol-function 'pop-to-buffer-same-window)
                    (lambda (buffer &rest _) (set-buffer buffer))))
           ,@body)
       (when-let* ((buffer (get-buffer gnosis-dashboard-buffer-name)))
         (kill-buffer buffer)))))

(defmacro gnosis-test-ewoc-dashboard--with-records (total &rest body)
  "Open a deterministic dashboard with TOTAL records, then evaluate BODY."
  (declare (indent 1) (debug (form body)))
  `(let ((pages (gnosis-test-ewoc-dashboard--pages ,total)))
     (gnosis-test-ewoc-dashboard--with-client (lambda (&rest _) (pop pages))
       (gnosis-dashboard)
       (gnosis-test-ewoc-dashboard--drain 'jobs)
       ,@body)))

(ert-deftest gnosis-dashboard-default-journey-is-summary-plus-ewoc-catalog ()
  (let ((pages (list (gnosis-test-ewoc-dashboard--page 0 2 "thema-000001")
                     (gnosis-test-ewoc-dashboard--page 2 1))))
    (gnosis-test-ewoc-dashboard--with-client (lambda (&rest _) (pop pages))
      (gnosis-dashboard)
      (with-current-buffer gnosis-dashboard-buffer-name
        (should (derived-mode-p 'gnosis-dashboard-mode))
        (should (ewoc-p gnosis-themata--ewoc))
        (should (string-search "Gnosis Dashboard" (buffer-string)))
        (should (string-search "Loading complete thema catalog" (buffer-string))))
      (gnosis-test-ewoc-dashboard--drain 'jobs)
      (with-current-buffer gnosis-dashboard-buffer-name
        (should (string-search "Question 0" (buffer-string)))
        (should (string-search "Question 2" (buffer-string)))
        (should (= 3 (hash-table-count gnosis-themata--nodes)))
        (should (= 3 (length (ewoc-collect gnosis-themata--ewoc #'identity))))
        (should
         (< (line-number-at-pos (ewoc-location (ewoc-nth gnosis-themata--ewoc 0)))
            (line-number-at-pos (ewoc-location (ewoc-nth gnosis-themata--ewoc 1)))))))))

(ert-deftest gnosis-dashboard-authority-change-replaces-catalog ()
  (let ((token (make-string 40 ?a))
        (pages (list (gnosis-test-ewoc-dashboard--page 0 1)))
        (catalog-calls 0))
    (gnosis-test-ewoc-dashboard--with-client
        (lambda (&rest _)
          (cl-incf catalog-calls)
          (pop pages))
      (cl-letf (((symbol-function 'gnosis-core--token) (lambda () token))
                ((symbol-function 'gnosis-core--authenticated-authority)
                 (lambda (&optional supplied-token)
                   (vector "https://test.example"
                           (secure-hash 'sha256 (or supplied-token token))))))
        (gnosis-dashboard)
        (gnosis-test-ewoc-dashboard--drain 'jobs)
        (setq token (make-string 40 ?b)
              pages (list (gnosis-test-ewoc-dashboard--page 100 1)))
        (gnosis-dashboard)
        (gnosis-test-ewoc-dashboard--drain 'jobs))
      (with-current-buffer gnosis-dashboard-buffer-name
        (should (= 2 catalog-calls))
        (should-not (string-search "Question 0" (buffer-string)))
        (should (string-search "Question 100" (buffer-string)))
        (should (= 1 (length (ewoc-collect gnosis-themata--ewoc #'identity))))))))

(ert-deftest gnosis-dashboard-ewoc-data-owns-complete-records ()
  (let ((pages (list (gnosis-test-ewoc-dashboard--page 0 2))))
    (gnosis-test-ewoc-dashboard--with-client (lambda (&rest _) (pop pages))
      (gnosis-dashboard)
      (gnosis-test-ewoc-dashboard--drain 'jobs)
      (with-current-buffer gnosis-dashboard-buffer-name
        (should-not (boundp 'gnosis-themata--records))
        (should-not (boundp 'gnosis-themata--order))
        (let* ((node (ewoc-nth gnosis-themata--ewoc 0))
               (record (ewoc-data node))
               (updated (plist-put (copy-tree record) :keimenon "Zulu EWOC")))
          (should (equal "thema-000000" (plist-get record :thema_id)))
          (should (equal "Question 0" (plist-get record :keimenon)))
          (ewoc-set-data node updated)
          (ewoc-invalidate gnosis-themata--ewoc node)
          (should (string-search "Zulu EWOC" (buffer-string))))
        (gnosis-themata-filter-search "Question 1")
        (gnosis-test-ewoc-dashboard--drain 'jobs)
        (should (= 2 (length (ewoc-collect gnosis-themata--ewoc #'identity))))
        (should-not (string-search "Zulu EWOC" (buffer-string)))
        (should (string-search "Question 1" (buffer-string)))
        (gnosis-themata-filter-search "")
        (gnosis-test-ewoc-dashboard--drain 'jobs)
        (gnosis-themata-sort 0)
        (gnosis-test-ewoc-dashboard--drain 'jobs)
        (should (equal "thema-000001"
                       (plist-get (ewoc-data (ewoc-nth gnosis-themata--ewoc 0))
                                  :thema_id)))))))

(ert-deftest gnosis-dashboard-max-review-filter-combines-with-live-projection ()
  (let* ((first (gnosis-test-ewoc-dashboard--record 0))
         (second (plist-put (gnosis-test-ewoc-dashboard--record 1)
                            :review_count 1))
         (third (plist-put (gnosis-test-ewoc-dashboard--record 2)
                           :review_count 2))
         (pages (list `(:generation "g1" :items [,first ,second ,third]
                        :next ,gnosis-core--json-null))))
    (gnosis-test-ewoc-dashboard--with-client (lambda (&rest _) (pop pages))
      (gnosis-dashboard)
      (gnosis-test-ewoc-dashboard--drain 'jobs)
      (with-current-buffer gnosis-dashboard-buffer-name
        (gnosis-themata-filter-search "Question")
        (gnosis-test-ewoc-dashboard--drain 'jobs)
        (gnosis-themata-sort 0)
        (gnosis-test-ewoc-dashboard--drain 'jobs)
        (should (gnosis-themata--goto-id "thema-000000"))
        (let ((current-prefix-arg nil))
          (call-interactively #'gnosis-themata-filter-max-reviews))
        (gnosis-test-ewoc-dashboard--drain 'jobs)
        (should (= gnosis-themata--max-reviews 0))
        (should (equal "Question" gnosis-themata--search-filter))
        (should (equal '(0) gnosis-themata--sort-key))
        (should (equal "thema-000000" (gnosis-themata--current-id)))
        (should (string-search "Question 0" (buffer-string)))
        (should-not (string-search "Question 1" (buffer-string)))
        (cl-letf (((symbol-function 'read-number)
                   (lambda (&rest _) 1)))
          (let ((current-prefix-arg '(4)))
            (call-interactively #'gnosis-themata-filter-max-reviews)))
        (gnosis-test-ewoc-dashboard--drain 'jobs)
        (should (= gnosis-themata--max-reviews 1))
        (should (string-search "Question 1" (buffer-string)))
        (should-not (string-search "Question 2" (buffer-string)))
        (gnosis-themata-filter-search "Question 1")
        (gnosis-test-ewoc-dashboard--drain 'jobs)
        (should-not (string-search "Question 0" (buffer-string)))
        (should (string-search "Question 1" (buffer-string)))))))

(ert-deftest gnosis-dashboard-sort-reset-restores-server-order ()
  (let* ((first (gnosis-test-ewoc-dashboard--record 1))
         (second (gnosis-test-ewoc-dashboard--record 0))
         (pages (list `(:generation "g1" :items [,first ,second]
                        :next ,gnosis-core--json-null))))
    (gnosis-test-ewoc-dashboard--with-client (lambda (&rest _) (pop pages))
      (gnosis-dashboard)
      (gnosis-test-ewoc-dashboard--drain 'jobs)
      (with-current-buffer gnosis-dashboard-buffer-name
        (should (equal '("thema-000001" "thema-000000")
                       (gnosis-test-ewoc-dashboard--ids)))
        (should (equal '(0 1)
                       (mapcar (lambda (record)
                                 (plist-get record :catalog-ordinal))
                               (ewoc-collect gnosis-themata--ewoc #'identity))))
        (gnosis-themata-sort 0)
        (gnosis-test-ewoc-dashboard--drain 'jobs)
        (should (equal '("thema-000000" "thema-000001")
                       (gnosis-test-ewoc-dashboard--ids)))
        (gnosis-themata-sort -1)
        (gnosis-test-ewoc-dashboard--drain 'jobs)
        (should (equal '("thema-000001" "thema-000000")
                       (gnosis-test-ewoc-dashboard--ids)))))))

(ert-deftest gnosis-dashboard-progressive-render-restores-unchanged-context ()
  (let ((pages (gnosis-test-ewoc-dashboard--pages 501)))
    (gnosis-test-ewoc-dashboard--with-client (lambda (&rest _) (pop pages))
      (gnosis-dashboard)
      (gnosis-test-ewoc-dashboard--drain 'jobs)
      (with-current-buffer gnosis-dashboard-buffer-name
        (should (gnosis-themata--goto-id "thema-000042"))
        (move-to-column 7)
        (let ((column (current-column)))
          (gnosis-themata-sort 0)
          (should jobs)
          (gnosis-test-ewoc-dashboard--drain 'jobs)
          (should (equal "thema-000042" (gnosis-themata--current-id)))
          (should (= column (current-column))))))))

(ert-deftest gnosis-dashboard-progressive-render-preserves-intervening-movement ()
  (let ((pages (gnosis-test-ewoc-dashboard--pages 501)))
    (gnosis-test-ewoc-dashboard--with-client (lambda (&rest _) (pop pages))
      (gnosis-dashboard)
      (gnosis-test-ewoc-dashboard--drain 'jobs)
      (with-current-buffer gnosis-dashboard-buffer-name
        (should (gnosis-themata--goto-id "thema-000000"))
        (move-to-column 5)
        (gnosis-themata-sort 0)
        (should jobs)
        (should (gnosis-themata--goto-id "thema-000001"))
        (move-to-column 9)
        (let ((column (current-column)))
          (gnosis-test-ewoc-dashboard--drain 'jobs)
          (should (equal "thema-000001" (gnosis-themata--current-id)))
          (should (= column (current-column))))))))

(ert-deftest gnosis-dashboard-progressive-render-drops-only-moved-selection ()
  (gnosis-test-ewoc-dashboard--with-records 1001
    (save-window-excursion
      (let ((buffer (get-buffer gnosis-dashboard-buffer-name))
            (window (selected-window)))
        (set-window-buffer window buffer)
        (with-current-buffer buffer
          (should (gnosis-themata--goto-id "thema-000042"))
          (set-window-start
           window (ewoc-location (gethash "thema-000020" gnosis-themata--nodes)) t)
          (gnosis-themata-sort 0)
          (should jobs)
          (should (gnosis-themata--goto-id "thema-000043"))
          (gnosis-test-ewoc-dashboard--drain 'jobs)
          (should (equal "thema-000043" (gnosis-themata--current-id)))
          (should (equal "thema-000020"
                         (gnosis-test-ewoc-dashboard--window-start-id window))))))))

(ert-deftest gnosis-dashboard-progressive-render-drops-only-moved-viewport ()
  (gnosis-test-ewoc-dashboard--with-records 1001
    (save-window-excursion
      (let ((buffer (get-buffer gnosis-dashboard-buffer-name))
            (window (selected-window)))
        (set-window-buffer window buffer)
        (with-current-buffer buffer
          (should (gnosis-themata--goto-id "thema-000042"))
          (move-to-column 7)
          (set-window-start
           window (ewoc-location (gethash "thema-000020" gnosis-themata--nodes)) t)
          (let ((column (current-column)))
            (gnosis-themata-sort 0)
            (should jobs)
            (let* ((moved-node (ewoc-nth gnosis-themata--ewoc 30))
                   (moved-id (plist-get (ewoc-data moved-node) :thema_id)))
              (set-window-start window (ewoc-location moved-node) t)
              (gnosis-test-ewoc-dashboard--drain 'jobs)
              (should (equal "thema-000042" (gnosis-themata--current-id)))
              (should (= column (current-column)))
              (should (equal moved-id
                             (gnosis-test-ewoc-dashboard--window-start-id
                              window))))))))))

(ert-deftest gnosis-dashboard-progressive-render-does-not-retarget-replaced-window ()
  (gnosis-test-ewoc-dashboard--with-records 1001
    (save-window-excursion
      (delete-other-windows)
      (let* ((buffer (get-buffer gnosis-dashboard-buffer-name))
             (target (selected-window))
             (other (split-window-right))
             (scratch (get-buffer-create " *gnosis-window-owner-test*")))
        (unwind-protect
            (progn
              (set-window-buffer target buffer)
              (set-window-buffer other buffer)
              (with-selected-window target
                (with-current-buffer buffer
                  (should (gnosis-themata--goto-id "thema-000042"))
                  (set-window-start
                   target
                   (ewoc-location (gethash "thema-000020" gnosis-themata--nodes))
                   t)
                  (gnosis-themata-sort 0)
                  (should jobs)
                  (let* ((other-node (ewoc-nth gnosis-themata--ewoc 30))
                         (other-id (plist-get (ewoc-data other-node) :thema_id)))
                    (set-window-start other (ewoc-location other-node) t)
                    (set-window-buffer target scratch)
                    (gnosis-test-ewoc-dashboard--drain 'jobs)
                    (should (equal "thema-000042" (gnosis-themata--current-id)))
                    (should (equal other-id
                                   (gnosis-test-ewoc-dashboard--window-start-id
                                    other)))))))
          (kill-buffer scratch))))))

(ert-deftest gnosis-dashboard-progressive-render-tolerates-deleted-window ()
  (gnosis-test-ewoc-dashboard--with-records 1001
    (save-window-excursion
      (delete-other-windows)
      (let* ((buffer (get-buffer gnosis-dashboard-buffer-name))
             (other (selected-window))
             (target (split-window-right))
             (scratch (get-buffer-create " *gnosis-deleted-window-test*")))
        (unwind-protect
            (progn
              (set-window-buffer other scratch)
              (set-window-buffer target buffer)
              (with-current-buffer buffer
                (should (gnosis-themata--goto-id "thema-000042"))
                (gnosis-themata-sort 0)
                (should jobs)
                (delete-window target)
                (should (gnosis-themata--goto-id "thema-000042"))
                (gnosis-test-ewoc-dashboard--drain 'jobs)
                (should (equal "thema-000042"
                               (gnosis-themata--current-id)))))
          (kill-buffer scratch))))))

(ert-deftest gnosis-dashboard-progressive-render-needs-no-visible-window ()
  (gnosis-test-ewoc-dashboard--with-records 1001
    (let ((buffer (get-buffer gnosis-dashboard-buffer-name)))
      (should-not (get-buffer-window buffer))
      (with-current-buffer buffer
        (should (gnosis-themata--goto-id "thema-000042"))
        (gnosis-themata-sort 0)
        (gnosis-test-ewoc-dashboard--drain 'jobs)
        (should (equal "thema-000042" (gnosis-themata--current-id)))))))

(ert-deftest gnosis-dashboard-sort-replacement-retains-pending-records ()
  (gnosis-test-ewoc-dashboard--with-records 1001
    (save-window-excursion
      (let ((buffer (get-buffer gnosis-dashboard-buffer-name))
            (window (selected-window)))
        (set-window-buffer window buffer)
        (with-current-buffer buffer
          (should (gnosis-themata--goto-id "thema-000042"))
          (set-window-start
           window (ewoc-location (gethash "thema-000020" gnosis-themata--nodes)) t)
          (gnosis-themata-sort 0)
          (let ((stale (pop jobs)))
            (gnosis-themata-sort 0)
            (let ((run gnosis-themata--render-run)
                  (count (hash-table-count gnosis-themata--nodes)))
              (apply (car stale) (cdr stale))
              (should (equal run gnosis-themata--render-run))
              (should (= count (hash-table-count gnosis-themata--nodes))))
            (gnosis-test-ewoc-dashboard--drain 'jobs)
            (gnosis-test-ewoc-dashboard--assert-complete-records 1001)
            (should (equal "thema-000042" (gnosis-themata--current-id)))
            (should (equal "thema-000020"
                           (gnosis-test-ewoc-dashboard--window-start-id
                            window)))))))))

(ert-deftest gnosis-dashboard-filter-replacement-retains-pending-records ()
  (gnosis-test-ewoc-dashboard--with-records 1001
    (with-current-buffer gnosis-dashboard-buffer-name
      (gnosis-themata-sort 0)
      (let ((stale (pop jobs)))
        (gnosis-themata-filter-search "Question 1")
        (apply (car stale) (cdr stale))
        (gnosis-test-ewoc-dashboard--drain 'jobs)
        (gnosis-test-ewoc-dashboard--assert-complete-records 1001)))))

(ert-deftest gnosis-dashboard-layout-replacement-retains-pending-records ()
  (gnosis-test-ewoc-dashboard--with-records 1001
    (with-current-buffer gnosis-dashboard-buffer-name
      (gnosis-themata-sort 0)
      (let ((stale (pop jobs)))
        (gnosis-themata--apply-layout
         (current-buffer) gnosis-themata--incarnation
         (1+ gnosis-themata--layout-width))
        (apply (car stale) (cdr stale))
        (gnosis-test-ewoc-dashboard--drain 'jobs)
        (gnosis-test-ewoc-dashboard--assert-complete-records 1001)))))

(ert-deftest gnosis-dashboard-mutation-replaces-pending-record-during-render ()
  (gnosis-test-ewoc-dashboard--with-records 1001
    (with-current-buffer gnosis-dashboard-buffer-name
      (setq gnosis-themata--search-filter "Question")
      (should (gnosis-themata--goto-id "thema-000042"))
      (gnosis-themata-sort 0)
      (let* ((stale (pop jobs))
             (updated (gnosis-test-ewoc-dashboard--record 1000)))
        (setq updated (plist-put updated :keimenon "Changed pending"))
        (gnosis-themata--replace-record updated)
        (apply (car stale) (cdr stale))
        (gnosis-test-ewoc-dashboard--drain 'jobs)
        (gnosis-test-ewoc-dashboard--assert-complete-records 1001)
        (should (equal "Changed pending"
                       (plist-get
                        (ewoc-data (gethash "thema-001000"
                                            gnosis-themata--nodes))
                        :keimenon)))
        (should (= 1000
                   (plist-get
                    (ewoc-data (gethash "thema-001000" gnosis-themata--nodes))
                    :catalog-ordinal)))
        (should (equal "thema-000042" (gnosis-themata--current-id)))
        (should (equal "Question" gnosis-themata--search-filter))
        (should gnosis-themata--sort-key)))))

(ert-deftest gnosis-dashboard-render-run-replacement-rejects-stale-callback ()
  (gnosis-test-ewoc-dashboard--with-records 1001
    (with-current-buffer gnosis-dashboard-buffer-name
      (gnosis-themata-sort 0)
      (let ((stale (pop jobs))
            (count (hash-table-count gnosis-themata--nodes)))
        (gnosis-themata--cancel-render)
        (apply (car stale) (cdr stale))
        (should (= count (hash-table-count gnosis-themata--nodes)))))))

(ert-deftest gnosis-dashboard-render-incarnation-replacement-rejects-stale-callback ()
  (gnosis-test-ewoc-dashboard--with-records 1001
    (with-current-buffer gnosis-dashboard-buffer-name
      (gnosis-themata-sort 0)
      (let ((stale (pop jobs))
            (count (hash-table-count gnosis-themata--nodes)))
        (setq gnosis-themata--incarnation (cl-incf gnosis-themata--sequence))
        (apply (car stale) (cdr stale))
        (should (= count (hash-table-count gnosis-themata--nodes)))))))

(ert-deftest gnosis-dashboard-render-authority-replacement-rejects-stale-callback ()
  (gnosis-test-ewoc-dashboard--with-records 1001
    (with-current-buffer gnosis-dashboard-buffer-name
      (gnosis-themata-sort 0)
      (let ((stale (pop jobs))
            (count (hash-table-count gnosis-themata--nodes)))
        (setq gnosis-themata--authority ["https://changed.example" "owner"])
        (apply (car stale) (cdr stale))
        (should (= count (hash-table-count gnosis-themata--nodes)))))))

(ert-deftest gnosis-dashboard-terminal-traversal-has-no-total-ceiling ()
  (let ((next-index 0)
        (calls 0)
        (max-page-size 0))
    (gnosis-test-ewoc-dashboard--with-client
        (lambda (&rest _)
          (cl-incf calls)
          (let* ((remaining (- 1001 next-index))
                 (count (min 100 remaining))
                 (start next-index)
                 (next (if (> remaining 100)
                           (format "thema-%06d" (+ next-index count -1))
                         gnosis-core--json-null)))
            (cl-incf next-index count)
            (setq max-page-size (max max-page-size count))
            (gnosis-test-ewoc-dashboard--page start count next)))
      (gnosis-dashboard)
      (gnosis-test-ewoc-dashboard--drain 'jobs)
      (with-current-buffer gnosis-dashboard-buffer-name
        (let ((records (ewoc-collect gnosis-themata--ewoc #'identity)))
          (should (= 1001 (length records)))
          (should (equal "thema-001000"
                         (plist-get (car (last records)) :thema_id)))))
      (should (= 11 calls))
      (should (<= max-page-size 100)))))

(ert-deftest gnosis-dashboard-formats-at-most-one-server-page-per-callback ()
  (let ((next-index 0)
        (pretty-printer-calls 0)
        (max-pretty-printer-calls 0))
    (gnosis-test-ewoc-dashboard--with-client
        (lambda (&rest _)
          (let* ((remaining (- 201 next-index))
                 (count (min 100 remaining))
                 (start next-index)
                 (next (if (> remaining 100)
                           (format "thema-%06d" (+ next-index count -1))
                         gnosis-core--json-null)))
            (cl-incf next-index count)
            (gnosis-test-ewoc-dashboard--page start count next)))
      (cl-letf (((symbol-function 'gnosis-themata--pretty-print)
                 (lambda (data)
                   (cl-incf pretty-printer-calls)
                   (gnosis-themata--pretty-print-default data))))
        (gnosis-dashboard)
        (while jobs
          (setq pretty-printer-calls 0)
          (let ((job (pop jobs)))
            (apply (car job) (cdr job)))
          (setq max-pretty-printer-calls
                (max max-pretty-printer-calls pretty-printer-calls))))
      (should (= 201 next-index))
      (should (<= max-pretty-printer-calls 100)))))

(ert-deftest gnosis-dashboard-pagination-rejects-empty-and-repeated-continuations ()
  (dolist (page `((:generation "g1" :items [] :next "same")
                  (:generation "g1"
                   :items [,(gnosis-test-ewoc-dashboard--record 0)]
                   :next "same")))
    (let ((calls 0))
      (gnosis-test-ewoc-dashboard--with-client
          (lambda (&rest _)
            (cl-incf calls)
            (if (= calls 1)
                page
              `(:generation "g1"
                :items [,(gnosis-test-ewoc-dashboard--record 1)]
                :next "same")))
        (gnosis-dashboard)
        (gnosis-test-ewoc-dashboard--drain 'jobs)
        (with-current-buffer gnosis-dashboard-buffer-name
          (should (string-search "catalog unavailable" (buffer-string)))
          (should-not gnosis-themata--loading-p))))))

(ert-deftest gnosis-dashboard-pagination-rejects-duplicates-without-publishing-them ()
  (let ((pages
         (list
          (gnosis-test-ewoc-dashboard--page 0 1 "thema-000000")
          `(:generation "g1"
            :items [,(gnosis-test-ewoc-dashboard--record 0)]
            :next ,gnosis-core--json-null))))
    (gnosis-test-ewoc-dashboard--with-client (lambda (&rest _) (pop pages))
      (gnosis-dashboard)
      (gnosis-test-ewoc-dashboard--drain 'jobs)
      (with-current-buffer gnosis-dashboard-buffer-name
        (should (= 1 (hash-table-count gnosis-themata--nodes)))
        (should (string-search "catalog unavailable" (buffer-string)))))))

(ert-deftest gnosis-dashboard-generation-conflict-is-explicit-and-retains-pages ()
  (let ((pages
         (list (gnosis-test-ewoc-dashboard--page 0 1 "thema-000000" "g1")
               (gnosis-test-ewoc-dashboard--page 1 1 nil "g2"))))
    (gnosis-test-ewoc-dashboard--with-client (lambda (&rest _) (pop pages))
      (gnosis-dashboard)
      (gnosis-test-ewoc-dashboard--drain 'jobs)
      (with-current-buffer gnosis-dashboard-buffer-name
        (should (= 1 (hash-table-count gnosis-themata--nodes)))
        (should (string-search "catalog changed during refresh" (buffer-string)))
        (should-not gnosis-themata--loading-p)))))

(ert-deftest gnosis-dashboard-reopen-error-retains-live-ewoc ()
  (let ((pages (list (gnosis-test-ewoc-dashboard--page 0 1))))
    (gnosis-test-ewoc-dashboard--with-client (lambda (&rest _) (pop pages))
      (gnosis-dashboard)
      (gnosis-test-ewoc-dashboard--drain 'jobs)
      (cl-letf (((symbol-function 'gnosis-core-dashboard)
                 (lambda () (signal 'gnosis-core-unavailable '("private")))))
        (gnosis-dashboard))
      (with-current-buffer gnosis-dashboard-buffer-name
        (should (= 1 (hash-table-count gnosis-themata--nodes)))
        (should (string-search "Question 0" (buffer-string)))
        (should (string-search "gnosisd unavailable" (buffer-string)))))))

(ert-deftest gnosis-dashboard-restart-cancels-stale-page-callback ()
  (let ((pages (list (gnosis-test-ewoc-dashboard--page 0 1))))
    (gnosis-test-ewoc-dashboard--with-client (lambda (&rest _) (pop pages))
      (gnosis-dashboard)
      (let ((stale (pop jobs)))
        (with-current-buffer gnosis-dashboard-buffer-name
          (gnosis-themata-refresh))
        (apply (car stale) (cdr stale)))
      (gnosis-test-ewoc-dashboard--drain 'jobs)
      (with-current-buffer gnosis-dashboard-buffer-name
        (should (= 1 (hash-table-count gnosis-themata--nodes)))
        (should-not gnosis-themata--loading-p)))))

(ert-deftest gnosis-dashboard-explicit-empty-and-empty-filter-states ()
  (gnosis-test-ewoc-dashboard--with-client
      (lambda (&rest _) (gnosis-test-ewoc-dashboard--page 0 0))
    (gnosis-dashboard)
    (gnosis-test-ewoc-dashboard--drain 'jobs)
    (with-current-buffer gnosis-dashboard-buffer-name
      (should (string-search "No themata yet. Press a to add one." (buffer-string)))))
  (gnosis-test-ewoc-dashboard--with-client
      (lambda (&rest _) (gnosis-test-ewoc-dashboard--page 0 1))
    (gnosis-dashboard)
    (gnosis-test-ewoc-dashboard--drain 'jobs)
    (with-current-buffer gnosis-dashboard-buffer-name
      (gnosis-themata-filter-search "does not exist")
      (gnosis-test-ewoc-dashboard--drain 'jobs)
      (should (string-search "No themata match current filters." (buffer-string))))))

(ert-deftest gnosis-dashboard-refresh-preserves-ewoc-selection-column-and-window ()
  (let ((pages (list (gnosis-test-ewoc-dashboard--page 0 3))))
    (gnosis-test-ewoc-dashboard--with-client (lambda (&rest _) (pop pages))
      (gnosis-dashboard)
      (gnosis-test-ewoc-dashboard--drain 'jobs)
      (save-window-excursion
        (let ((buffer (get-buffer gnosis-dashboard-buffer-name)))
          (set-window-buffer (selected-window) buffer)
          (with-current-buffer buffer
            (should (gnosis-themata--goto-id "thema-000000"))
            (set-window-start (selected-window) (point))
            (should (gnosis-themata--goto-id "thema-000001"))
            (move-to-column 7)
            (let ((column (current-column)))
              (setq pages (list (gnosis-test-ewoc-dashboard--page 0 3)))
              (gnosis-themata-refresh)
              (gnosis-test-ewoc-dashboard--drain 'jobs)
              (should (equal "thema-000001" (gnosis-themata--current-id)))
              (should (= column (current-column)))
              (should
               (equal "thema-000000"
                      (plist-get
                       (ewoc-data
                        (ewoc-locate gnosis-themata--ewoc
                                     (window-start (selected-window))))
                       :thema_id))))))))))

(ert-deftest gnosis-dashboard-refresh-preserves-selection-moved-after-dispatch ()
  (gnosis-test-ewoc-dashboard--with-records 501
    (with-current-buffer gnosis-dashboard-buffer-name
      (should (gnosis-themata--goto-id "thema-000042"))
      (setq pages (gnosis-test-ewoc-dashboard--pages 501))
      (gnosis-themata-refresh)
      (should jobs)
      (should (gnosis-themata--goto-id "thema-000043"))
      (move-to-column 9)
      (let ((column (current-column)))
        (gnosis-test-ewoc-dashboard--drain 'jobs)
        (should (equal "thema-000043" (gnosis-themata--current-id)))
        (should (= column (current-column)))))))

(ert-deftest gnosis-dashboard-refresh-preserves-viewport-moved-after-dispatch ()
  (gnosis-test-ewoc-dashboard--with-records 501
    (save-window-excursion
      (let ((buffer (get-buffer gnosis-dashboard-buffer-name))
            (window (selected-window)))
        (set-window-buffer window buffer)
        (with-current-buffer buffer
          (should (gnosis-themata--goto-id "thema-000042"))
          (set-window-start
           window (ewoc-location (gethash "thema-000020" gnosis-themata--nodes)) t)
          (setq pages (gnosis-test-ewoc-dashboard--pages 501))
          (gnosis-themata-refresh)
          (set-window-start
           window (ewoc-location (gethash "thema-000030" gnosis-themata--nodes)) t)
          (gnosis-test-ewoc-dashboard--drain 'jobs)
          (should (equal "thema-000042" (gnosis-themata--current-id)))
          (should (equal "thema-000030"
                         (gnosis-test-ewoc-dashboard--window-start-id window))))))))

(ert-deftest gnosis-dashboard-refresh-missing-selected-row-falls-back-only-while-owned ()
  (let ((pages (list (gnosis-test-ewoc-dashboard--page 0 3))))
    (gnosis-test-ewoc-dashboard--with-client (lambda (&rest _) (pop pages))
      (gnosis-dashboard)
      (gnosis-test-ewoc-dashboard--drain 'jobs)
      (with-current-buffer gnosis-dashboard-buffer-name
        (should (gnosis-themata--goto-id "thema-000001"))
        (setq pages (list (gnosis-test-ewoc-dashboard--page 100 3)))
        (gnosis-themata-refresh)
        (gnosis-test-ewoc-dashboard--drain 'jobs)
        (should (equal "thema-000100" (gnosis-themata--current-id))))))
  (let ((pages (list (gnosis-test-ewoc-dashboard--page 0 3))))
    (gnosis-test-ewoc-dashboard--with-client (lambda (&rest _) (pop pages))
      (gnosis-dashboard)
      (gnosis-test-ewoc-dashboard--drain 'jobs)
      (with-current-buffer gnosis-dashboard-buffer-name
        (should (gnosis-themata--goto-id "thema-000001"))
        (setq pages (list (gnosis-test-ewoc-dashboard--page 100 3)))
        (gnosis-themata-refresh)
        (should (gnosis-themata--goto-id "thema-000002"))
        (gnosis-test-ewoc-dashboard--drain 'jobs)
        (should-not (gnosis-themata--current-id))))))

(ert-deftest gnosis-dashboard-refresh-missing-start-row-has-no-viewport-fallback ()
  (let ((pages (list (gnosis-test-ewoc-dashboard--page 0 3))))
    (gnosis-test-ewoc-dashboard--with-client (lambda (&rest _) (pop pages))
      (gnosis-dashboard)
      (gnosis-test-ewoc-dashboard--drain 'jobs)
      (save-window-excursion
        (let ((buffer (get-buffer gnosis-dashboard-buffer-name))
              (window (selected-window)))
          (set-window-buffer window buffer)
          (with-current-buffer buffer
            (should (gnosis-themata--goto-id "thema-000001"))
            (set-window-start
             window
             (ewoc-location (gethash "thema-000000" gnosis-themata--nodes)) t)
            (setq pages (list (gnosis-test-ewoc-dashboard--page 100 3)))
            (gnosis-themata-refresh)
            (gnosis-test-ewoc-dashboard--drain 'jobs)
            (should (equal "thema-000100" (gnosis-themata--current-id)))
            (should-not
             (equal "thema-000100"
                    (gnosis-test-ewoc-dashboard--window-start-id window)))))))))

(ert-deftest gnosis-dashboard-surgical-ewoc-update-preserves-selected-id ()
  (let ((pages (list (gnosis-test-ewoc-dashboard--page 0 3))))
    (gnosis-test-ewoc-dashboard--with-client (lambda (&rest _) (pop pages))
      (gnosis-dashboard)
      (gnosis-test-ewoc-dashboard--drain 'jobs)
      (with-current-buffer gnosis-dashboard-buffer-name
        (gnosis-themata--goto-id "thema-000002")
        (let ((updated (copy-tree
                        (ewoc-data (gethash "thema-000001"
                                            gnosis-themata--nodes)))))
          (setq updated (plist-put updated :keimenon "Changed surgically"))
          (gnosis-themata--replace-record updated))
        (should (equal "thema-000002" (gnosis-themata--current-id)))
        (should (string-search "Changed surgically" (buffer-string)))))))

(ert-deftest gnosis-dashboard-authoritative-mutations-update-ewoc-surgically ()
  (let ((pages (list (gnosis-test-ewoc-dashboard--page 0 2))))
    (gnosis-test-ewoc-dashboard--with-client (lambda (&rest _) (pop pages))
      (gnosis-dashboard)
      (gnosis-test-ewoc-dashboard--drain 'jobs)
      (with-current-buffer gnosis-dashboard-buffer-name
        (gnosis-themata--goto-id "thema-000001")
        (let ((ordinal (plist-get
                        (ewoc-data (gethash "thema-000001"
                                            gnosis-themata--nodes))
                        :catalog-ordinal)))
          (cl-letf (((symbol-function 'gnosis-edit-thema)
                     (lambda (id callback)
                       (funcall callback
                                `(:thema_id ,id :revision 2 :kind "basic"
                                  :keimenon "Edited" :hypothesis ["H"]
                                  :answer ["A"] :tags ["tag"]
                                  :archived ,gnosis-core--json-false)))))
            (gnosis-themata-edit))
          (should (string-search "Edited" (buffer-string)))
          (should (= ordinal
                     (plist-get
                      (ewoc-data (gethash "thema-000001"
                                          gnosis-themata--nodes))
                      :catalog-ordinal))))
        (cl-letf (((symbol-function 'y-or-n-p) (lambda (&rest _) t))
                  ((symbol-function 'gnosis-core-thema-set-archived)
                   (lambda (id _revision desired)
                     `(:thema_id ,id :revision 3 :kind "basic"
                       :keimenon "Edited" :hypothesis ["H"] :answer ["A"]
                       :tags ["tag"] :archived ,desired))))
          (gnosis-themata-toggle-archive))
        (should (eq t (plist-get
                       (ewoc-data (gethash "thema-000001" gnosis-themata--nodes))
                       :archived)))
        (cl-letf (((symbol-function 'y-or-n-p) (lambda (&rest _) t))
                  ((symbol-function 'gnosis-core-thema-set-suspended)
                   (lambda (_id _revision desired)
                     `(:state (:state_revision 3 :suspended ,desired)))))
          (gnosis-themata-toggle-suspension))
        (should (eq t (plist-get
                       (ewoc-data (gethash "thema-000001" gnosis-themata--nodes))
                       :suspended)))
        (cl-letf (((symbol-function 'gnosis-edit-read-kind) (lambda () "basic"))
                  ((symbol-function 'gnosis-add-thema)
                   (lambda (callback _kind)
                     (funcall callback
                              `(:thema_id "thema-new" :revision 1 :kind "basic"
                                :keimenon "Added" :hypothesis ["H"]
                                :answer ["A"] :tags []
                                :archived ,gnosis-core--json-false)))))
          (gnosis-themata-add))
        (should (gethash "thema-new" gnosis-themata--nodes))
        (should (= 2 (plist-get
                      (ewoc-data (gethash "thema-new" gnosis-themata--nodes))
                      :catalog-ordinal)))
        (should (equal "thema-000001" (gnosis-themata--current-id)))))))

(provide 'gnosis-test-ewoc-dashboard)
;;; gnosis-test-ewoc-dashboard.el ends here
